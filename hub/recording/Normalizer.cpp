#include "Normalizer.h"

namespace {

// Read full file into a planar AudioBuffer + sample rate. Returns nullptr on failure.
std::unique_ptr<juce::AudioBuffer<float>> readWavFile(const juce::File& f, double& sample_rate_out) {
    juce::AudioFormatManager mgr;
    mgr.registerBasicFormats();
    std::unique_ptr<juce::AudioFormatReader> reader(mgr.createReaderFor(f));
    if (!reader) return nullptr;

    sample_rate_out = reader->sampleRate;
    auto buffer = std::make_unique<juce::AudioBuffer<float>>(
        static_cast<int>(reader->numChannels),
        static_cast<int>(reader->lengthInSamples));
    reader->read(buffer.get(), 0,
                 static_cast<int>(reader->lengthInSamples), 0, true, true);
    return buffer;
}

// Apply linear gain in-place, then write to `out`. Always 32-bit float stereo.
bool writeWavWithGain(const juce::AudioBuffer<float>& src, double sample_rate,
                       float gain_lin, const juce::File& out) {
    if (out.existsAsFile()) out.deleteFile();

    juce::WavAudioFormat fmt;
    std::unique_ptr<juce::OutputStream> stream = out.createOutputStream();
    if (!stream) return false;

    const auto opts = juce::AudioFormatWriterOptions{}
                          .withSampleRate(sample_rate)
                          .withNumChannels(src.getNumChannels())
                          .withBitsPerSample(32)
                          .withSampleFormat(juce::AudioFormatWriterOptions::SampleFormat::floatingPoint);
    auto writer = fmt.createWriterFor(stream, opts);
    if (!writer) return false;

    juce::AudioBuffer<float> scaled(src.getNumChannels(), src.getNumSamples());
    for (int ch = 0; ch < src.getNumChannels(); ++ch) {
        scaled.copyFrom(ch, 0, src, ch, 0, src.getNumSamples());
    }
    scaled.applyGain(gain_lin);
    writer->writeFromAudioSampleBuffer(scaled, 0, scaled.getNumSamples());
    return true;
}

}  // namespace

OfflineNormalizer::OfflineNormalizer(std::vector<Task> tasks)
    : juce::Thread("GathererOfflineNormalizer"),
      tasks_(std::move(tasks))
{
    in_progress_.store(!tasks_.empty(), std::memory_order_release);
}

OfflineNormalizer::~OfflineNormalizer() {
    signalThreadShouldExit();
    stopThread(5000);
}

std::vector<OfflineNormalizer::Result> OfflineNormalizer::results() const {
    const juce::ScopedLock sl(results_lock_);
    return results_;
}

void OfflineNormalizer::run() {
    for (const auto& t : tasks_) {
        if (threadShouldExit()) break;

        Result r;
        r.source          = t.file;
        r.gain_applied_db = t.gain_db;

        double sr = 0.0;
        auto buf = readWavFile(t.file, sr);
        if (!buf || sr <= 0.0) {
            r.error = "Could not read file";
            const juce::ScopedLock sl(results_lock_);
            results_.push_back(std::move(r));
            continue;
        }

        const float gain_lin = std::pow(10.0f, t.gain_db / 20.0f);
        const auto out_file  = t.file.getSiblingFile(
            t.file.getFileNameWithoutExtension() + "_normalized.wav");

        if (!writeWavWithGain(*buf, sr, gain_lin, out_file)) {
            r.error = "Could not write normalized file";
            const juce::ScopedLock sl(results_lock_);
            results_.push_back(std::move(r));
            continue;
        }

        r.normalized = out_file;
        r.success    = true;
        {
            const juce::ScopedLock sl(results_lock_);
            results_.push_back(std::move(r));
        }
    }

    in_progress_.store(false, std::memory_order_release);
}
