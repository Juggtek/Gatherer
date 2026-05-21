#pragma once

#include <cstddef>
#include <string>

namespace gatherer {

// Cross-platform named shared memory region.
//
// Backed by POSIX shm_open + mmap on macOS/Linux, CreateFileMappingW + MapViewOfFile on Windows.
// The region persists until the last instance unmaps it; if this instance is the creator (owner),
// the OS-level name is unlinked at destruction (POSIX) or with the last handle close (Windows).
//
// `name` is a portable identifier; the implementation prepends the OS-specific prefix
// ("/" on POSIX, "Local\\" on Windows).
class SharedMemory {
public:
    enum class Mode {
        // Create new, fail if exists.
        CreateNew,
        // Open existing, fail if absent.
        OpenExisting,
        // Try open; create if absent. `isOwner()` reports which path was taken.
        OpenOrCreate,
    };

    // `size` is required for CreateNew / OpenOrCreate. For OpenExisting, pass the expected size;
    // it must match the existing region's size (or pass 0 to accept whatever size exists).
    SharedMemory(const std::string& name, std::size_t size, Mode mode);
    ~SharedMemory();

    SharedMemory(const SharedMemory&) = delete;
    SharedMemory& operator=(const SharedMemory&) = delete;
    SharedMemory(SharedMemory&&) noexcept;
    SharedMemory& operator=(SharedMemory&&) noexcept;

    void*       data() const noexcept;
    std::size_t size() const noexcept;
    bool        isOwner() const noexcept;
    const std::string& name() const noexcept;

    // Force-remove the named region from the OS. POSIX: shm_unlink. Win32: no-op (the kernel
    // refcounts handles and removes the name automatically).
    // Existing mappings in this or other processes remain valid until they unmap.
    static void unlink(const std::string& name) noexcept;

private:
    struct Impl;
    Impl* impl_;
};

} // namespace gatherer
