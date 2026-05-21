add_executable(poc_shm_writer poc_shm_writer.cpp)
target_link_libraries(poc_shm_writer PRIVATE gatherer-common)

add_executable(poc_shm_reader poc_shm_reader.cpp)
target_link_libraries(poc_shm_reader PRIVATE gatherer-common)

add_executable(poc_ring_writer poc_ring_writer.cpp)
target_link_libraries(poc_ring_writer PRIVATE gatherer-common)

add_executable(poc_ring_reader poc_ring_reader.cpp)
target_link_libraries(poc_ring_reader PRIVATE gatherer-common)
