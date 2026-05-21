juce_add_plugin(gatherer-sat
    COMPANY_NAME            "Gatherer"
    BUNDLE_ID               "com.gatherer.sat"
    IS_SYNTH                FALSE
    NEEDS_MIDI_INPUT        FALSE
    NEEDS_MIDI_OUTPUT       FALSE
    IS_MIDI_EFFECT          FALSE
    EDITOR_WANTS_KEYBOARD_FOCUS FALSE
    COPY_PLUGIN_AFTER_BUILD TRUE
    PLUGIN_MANUFACTURER_CODE Gthr
    PLUGIN_CODE              Gsat
    FORMATS                  VST3
    PRODUCT_NAME            "Gatherer Sat"
    VST3_CATEGORIES         "Fx"
)

juce_generate_juce_header(gatherer-sat)

target_sources(gatherer-sat PRIVATE
    PluginProcessor.cpp
    PluginEditor.cpp
)

target_compile_definitions(gatherer-sat PUBLIC
    JUCE_WEB_BROWSER=0
    JUCE_USE_CURL=0
    JUCE_VST3_CAN_REPLACE_VST2=0
    JUCE_DISPLAY_SPLASH_SCREEN=0
)

target_link_libraries(gatherer-sat
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
    set(VST3_BUNDLE_DIR "${CMAKE_BINARY_DIR}/satellite/gatherer-sat_artefacts/$<CONFIG>/VST3/Gatherer Sat.vst3")
    add_custom_command(TARGET gatherer-sat_VST3 POST_BUILD
        COMMAND codesign --force --deep --sign -
            "${VST3_INSTALL_DIR}/Gatherer Sat.vst3"
        COMMENT "Codesigning Gatherer Sat.vst3 (ad-hoc, for local Logic/Studio One load)"
    )
endif()
