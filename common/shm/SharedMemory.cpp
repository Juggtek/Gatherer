#include "SharedMemory.h"

#include <stdexcept>
#include <string>
#include <system_error>
#include <utility>

#if defined(_WIN32)
    #ifndef WIN32_LEAN_AND_MEAN
        #define WIN32_LEAN_AND_MEAN
    #endif
    #include <windows.h>
#else
    #include <fcntl.h>
    #include <sys/mman.h>
    #include <sys/stat.h>
    #include <unistd.h>
    #include <cerrno>
    #include <cstring>
#endif

namespace gatherer {

struct SharedMemory::Impl {
    std::string  name;
    void*        data    = nullptr;
    std::size_t  size    = 0;
    bool         owner   = false;
#if defined(_WIN32)
    HANDLE       handle  = nullptr;
#else
    int          fd      = -1;
#endif
};

namespace {

#if defined(_WIN32)

std::wstring osName(const std::string& name) {
    // "Local\\<name>" — session-scoped namespace.
    std::wstring prefixed = L"Local\\";
    prefixed.reserve(prefixed.size() + name.size());
    for (char c : name) prefixed.push_back(static_cast<wchar_t>(c));
    return prefixed;
}

[[noreturn]] void throwLastError(const char* what) {
    DWORD err = GetLastError();
    throw std::system_error(static_cast<int>(err), std::system_category(), what);
}

#else

std::string osName(const std::string& name) {
    // POSIX shm names must start with '/' and be short (macOS: PSHMNAMLEN-1 = 30 chars).
    return "/" + name;
}

[[noreturn]] void throwErrno(const char* what) {
    throw std::system_error(errno, std::generic_category(), what);
}

#endif

} // namespace

SharedMemory::SharedMemory(const std::string& name, std::size_t size, Mode mode)
    : impl_(new Impl{})
{
    impl_->name = name;

#if defined(_WIN32)
    const auto wname = osName(name);

    if (mode == Mode::OpenExisting) {
        impl_->handle = OpenFileMappingW(FILE_MAP_ALL_ACCESS, FALSE, wname.c_str());
        if (impl_->handle == nullptr) throwLastError("OpenFileMappingW");
        impl_->owner = false;
    } else {
        // CreateNew or OpenOrCreate — both go through CreateFileMappingW; we inspect
        // ERROR_ALREADY_EXISTS to decide ownership and to fail CreateNew.
        if (size == 0) throw std::invalid_argument("size must be > 0 when creating");
        DWORD sizeHi = static_cast<DWORD>(static_cast<uint64_t>(size) >> 32);
        DWORD sizeLo = static_cast<DWORD>(size & 0xFFFFFFFFu);
        impl_->handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE, nullptr, PAGE_READWRITE,
            sizeHi, sizeLo, wname.c_str());
        if (impl_->handle == nullptr) throwLastError("CreateFileMappingW");
        const bool alreadyExisted = (GetLastError() == ERROR_ALREADY_EXISTS);
        if (alreadyExisted && mode == Mode::CreateNew) {
            CloseHandle(impl_->handle);
            impl_->handle = nullptr;
            throw std::runtime_error("shared memory already exists");
        }
        impl_->owner = !alreadyExisted;
    }

    impl_->data = MapViewOfFile(impl_->handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
    if (impl_->data == nullptr) {
        CloseHandle(impl_->handle);
        impl_->handle = nullptr;
        throwLastError("MapViewOfFile");
    }

    if (size == 0) {
        MEMORY_BASIC_INFORMATION info{};
        if (VirtualQuery(impl_->data, &info, sizeof(info)) == 0) {
            UnmapViewOfFile(impl_->data);
            CloseHandle(impl_->handle);
            impl_->handle = nullptr;
            impl_->data = nullptr;
            throwLastError("VirtualQuery");
        }
        impl_->size = info.RegionSize;
    } else {
        impl_->size = size;
    }

#else
    const auto pname = osName(name);

    int flags = 0;
    bool tryCreateFirst = false;
    switch (mode) {
        case Mode::CreateNew:    flags = O_RDWR | O_CREAT | O_EXCL; tryCreateFirst = true; break;
        case Mode::OpenExisting: flags = O_RDWR;                     tryCreateFirst = false; break;
        case Mode::OpenOrCreate: flags = O_RDWR | O_CREAT | O_EXCL; tryCreateFirst = true; break;
    }

    impl_->fd = shm_open(pname.c_str(), flags, 0600);
    if (impl_->fd < 0) {
        if (mode == Mode::OpenOrCreate && errno == EEXIST) {
            impl_->fd = shm_open(pname.c_str(), O_RDWR, 0600);
            if (impl_->fd < 0) throwErrno("shm_open (existing)");
            impl_->owner = false;
            tryCreateFirst = false;
        } else {
            throwErrno("shm_open");
        }
    } else {
        impl_->owner = tryCreateFirst;
    }

    if (impl_->owner) {
        if (size == 0) {
            ::close(impl_->fd);
            shm_unlink(pname.c_str());
            impl_->fd = -1;
            throw std::invalid_argument("size must be > 0 when creating");
        }
        if (ftruncate(impl_->fd, static_cast<off_t>(size)) != 0) {
            int e = errno;
            ::close(impl_->fd);
            shm_unlink(pname.c_str());
            impl_->fd = -1;
            errno = e;
            throwErrno("ftruncate");
        }
        impl_->size = size;
    } else {
        // Opener: discover existing size if caller passed 0, otherwise trust caller.
        if (size == 0) {
            struct stat st{};
            if (fstat(impl_->fd, &st) != 0) {
                int e = errno; ::close(impl_->fd); impl_->fd = -1; errno = e;
                throwErrno("fstat");
            }
            impl_->size = static_cast<std::size_t>(st.st_size);
        } else {
            impl_->size = size;
        }
    }

    impl_->data = mmap(nullptr, impl_->size, PROT_READ | PROT_WRITE,
                       MAP_SHARED, impl_->fd, 0);
    if (impl_->data == MAP_FAILED) {
        int e = errno;
        ::close(impl_->fd);
        if (impl_->owner) shm_unlink(pname.c_str());
        impl_->fd = -1;
        impl_->data = nullptr;
        errno = e;
        throwErrno("mmap");
    }

    // The fd is no longer needed after mmap; the mapping holds the reference.
    ::close(impl_->fd);
    impl_->fd = -1;
#endif
}

SharedMemory::~SharedMemory() {
    if (!impl_) return;

#if defined(_WIN32)
    if (impl_->data)   UnmapViewOfFile(impl_->data);
    if (impl_->handle) CloseHandle(impl_->handle);
#else
    if (impl_->data) munmap(impl_->data, impl_->size);
    // NOTE: we intentionally do NOT shm_unlink on owner destruction.
    // Plugins are loaded and unloaded asynchronously by the DAW; if the creator
    // unlinks at destruction, a subsequent plugin instance loading later will
    // create a fresh shm under the same name, and the still-alive peer (with the
    // old mapping) won't see it. The named region persists for the process
    // lifetime; explicit cleanup uses SharedMemory::unlink(name) on demand.
#endif

    delete impl_;
    impl_ = nullptr;
}

SharedMemory::SharedMemory(SharedMemory&& other) noexcept
    : impl_(other.impl_)
{
    other.impl_ = nullptr;
}

SharedMemory& SharedMemory::operator=(SharedMemory&& other) noexcept {
    if (this != &other) {
        this->~SharedMemory();
        impl_ = other.impl_;
        other.impl_ = nullptr;
    }
    return *this;
}

void* SharedMemory::data() const noexcept {
    return impl_ ? impl_->data : nullptr;
}

std::size_t SharedMemory::size() const noexcept {
    return impl_ ? impl_->size : 0;
}

bool SharedMemory::isOwner() const noexcept {
    return impl_ && impl_->owner;
}

const std::string& SharedMemory::name() const noexcept {
    static const std::string empty;
    return impl_ ? impl_->name : empty;
}

void SharedMemory::unlink(const std::string& name) noexcept {
#if defined(_WIN32)
    (void)name;  // Win32 has no equivalent; kernel refcounts handles.
#else
    const std::string pname = "/" + name;
    shm_unlink(pname.c_str());
#endif
}

} // namespace gatherer
