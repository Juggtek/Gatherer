// Force-unlinks the Gatherer shared memory region. Useful when the DAW crashed
// or was force-killed and the shm has stale hub_uuid / slot state that prevents
// fresh registration.
//
// Safe to run while plugins are loaded — existing mappings remain valid, only the
// name is removed from the OS namespace, so no NEW attachers can find this region.
// Combine with quitting the DAW for a clean reset.

#include "protocol/SharedRegion.h"
#include "shm/SharedMemory.h"

#include <cstdio>

int main() {
    std::printf("Unlinking '%s' from the OS namespace...\n", gatherer::protocol::SHM_NAME);
    gatherer::SharedMemory::unlink(gatherer::protocol::SHM_NAME);
    std::printf("Done. Reload your DAW for a clean session.\n");
    return 0;
}
