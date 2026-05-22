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

// Write `src` (with linear gain applied) to `out`, optionally padded with
// leading and trailing silence so the file's total length is exactly
// `total_length_samples`. When `total_length_samples <= 0` the output is the
// same length as `src`. Always 32-bit float stereo.
bool writeWavAligned(const juce::AudioBuffer<float>& src, double sample_rate,
                     float gain_lin, std::int64_t offset_samples,
                     std::int64_t total_length_samples,
                     const juce::File& out) {
    if (out.existsAsFile()) out.deleteFile();

    juce::WavAudioFormat fmt;
    std::unique_ptr<juce::OutputStream> stream = out.createOutputStream();
    if (!stream) return false;

    const auto channels = src.getNumChannels();
    const auto opts = juce::AudioFormatWriterOptions{}
                          .withSampleRate(sample_rate)
                          .withNumChannels(channels)
                          .withBitsPerSample(32)
                          .withSampleFormat(juce::AudioFormatWriterOptions::SampleFormat::floatingPoint);
    auto writer = fmt.createWriterFor(stream, opts);
    if (!writer) return false;

    const auto src_n = src.getNumSamples();
    const auto out_n = (total_length_samples > 0)
        ? static_cast<int>(total_length_samples)
        : src_n;

    // Build the aligned output buffer: zeros + gained src + zeros.
    juce::AudioBuffer<float> output(channels, out_n);
    output.clear();

    const auto copy_start = static_cast<int>(std::max<std::int64_t>(0, offset_samples));
    if (copy_start < out_n) {
        const auto copy_len = std::min(src_n, out_n - copy_start);
        if (copy_len > 0) {
            for (int ch = 0; ch < channels; ++ch) {
                output.copyFrom(ch, copy_start, src, ch, 0, copy_len);
            }
        }
    }
    if (std::abs(gain_lin - 1.0f) > 1e-9f) {
        output.applyGain(gain_lin);
    }

    writer->writeFromAudioSampleBuffer(output, 0, out_n);
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
            t.file.getFileNameWithoutExtension() + t.output_suffix + ".wav");

        if (!writeWavAligned(*buf, sr, gain_lin,
                              t.offset_samples, t.total_length_samples,
                              out_file)) {
            r.error = "Could not write output file";
            const juce::ScopedLock sl(results_lock_);
            results_.push_back(std::move(r));
            continue;
        }

        r.output = out_file;
        r.success    = true;
        {
            const juce::ScopedLock sl(results_lock_);
            results_.push_back(std::move(r));
        }
    }

    in_progress_.store(false, std::memory_order_release);
}
