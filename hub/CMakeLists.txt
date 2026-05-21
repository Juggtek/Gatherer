juce_add_plugin(gatherer-hub
    COMPANY_NAME            "Gatherer"
    BUNDLE_ID               "com.gatherer.hub"
    IS_SYNTH                FALSE
    NEEDS_MIDI_INPUT        FALSE
    NEEDS_MIDI_OUTPUT       FALSE
    IS_MIDI_EFFECT          FALSE
    EDITOR_WANTS_KEYBOARD_FOCUS FALSE
    COPY_PLUGIN_AFTER_BUILD TRUE
    PLUGIN_MANUFACTURER_CODE Gthr
    PLUGIN_CODE              Ghub
    FORMATS                  VST3 Standalone
    PRODUCT_NAME            "Gatherer Hub"
    VST3_CATEGORIES         "Fx"
)

juce_generate_juce_header(gatherer-hub)

target_sources(gatherer-hub PRIVATE
    PluginProcessor.cpp
    PluginEditor.cpp
    LayerRow.cpp
    LayerLane.cpp
    recording/LayerWriter.cpp
    recording/Recorder.cpp
    recording/Normalizer.cpp
    undo/MixCommands.cpp
    undo/RecordingCommands.cpp
    playback/PlaybackEngine.cpp
    session/SessionManager.cpp
)

target_compile_definitions(gatherer-hub PUBLIC
    JUCE_WEB_BROWSER=0
    JUCE_USE_CURL=0
    JUCE_VST3_CAN_REPLACE_VST2=0
    JUCE_DISPLAY_SPLASH_SCREEN=0
)

target_link_libraries(gatherer-hub
    PRIVATE
        gatherer-common
        juce::juce_audio_utils
    PUBLIC
        juce::juce_recommended_config_flags
        juce::juce_recommended_lto_flags
        juce::juce_recommended_warning_flags
)

if(APPLE)
    set(VST3_INSTALL_DIR "$ENV{HOME}/Library/Audio/Plug-Ins/VST3")
    add_custom_command(TARGET gatherer-hub_VST3 POST_BUILD
        COMMAND codesign --force --deep --sign -
            "${VST3_INSTALL_DIR}/Gatherer Hub.vst3"
        COMMENT "Codesigning Gatherer Hub.vst3 (ad-hoc)"
    )
endif()
