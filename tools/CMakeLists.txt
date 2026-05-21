add_executable(gatherer-watch watcher.cpp)
target_link_libraries(gatherer-watch PRIVATE gatherer-common)

add_executable(gatherer-reset reset.cpp)
target_link_libraries(gatherer-reset PRIVATE gatherer-common)
