// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include <fuchsia/media/tuning/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstdint>
#include <memory>
#include <set>
#include <vector>

#include "src/media/audio/audio_core/audio_device.h"
#include "src/media/audio/audio_core/audio_tuner_impl.h"
#include "src/media/audio/audio_core/mixer/gain.h"
#include "src/media/audio/lib/analysis/analysis.h"
#include "src/media/audio/lib/analysis/generators.h"
#include "src/media/audio/lib/test/comparators.h"
#include "src/media/audio/lib/test/hermetic_audio_test.h"

using ASF = fuchsia::media::AudioSampleFormat;
using AudioRenderUsage = fuchsia::media::AudioRenderUsage;

namespace media::audio::test {

namespace {
constexpr zx::duration kPacketLength = zx::msec(10);
constexpr int64_t kNumPacketsInPayload = 50;
constexpr int64_t kDebugFramesPerPacket = 480;
// Tolerance to account for scheduling latency.
constexpr int64_t kToleranceInPackets = 2;
// The one-sided filter width of the SincSampler.
constexpr int64_t kSincSamplerHalfFilterWidth = 13;
// The length of gain ramp for each volume change.
// Must match the constant in audio_core.
constexpr zx::duration kVolumeRampDuration = zx::msec(5);
}  // namespace

template <ASF SampleType>
class AudioRendererPipelineTest : public HermeticAudioTest {
 protected:
  static constexpr int32_t kOutputFrameRate = 48000;
  static constexpr int32_t kNumChannels = 2;

  static constexpr int64_t PacketsToFrames(int64_t num_packets, int32_t frame_rate) {
    auto numerator = num_packets * frame_rate * kPacketLength.to_msecs();
    FX_CHECK(numerator % 1000 == 0);
    return numerator / 1000;
  }

  void SetUp() override {
    HermeticAudioTest::SetUp();
    // The output can store exactly 1s of audio data.
    auto format = Format::Create<SampleType>(2, kOutputFrameRate).take_value();
    output_ = CreateOutput({{0xff, 0x00}}, format, kOutputFrameRate);
  }

  void TearDown() override {
    // None of our tests should underflow.
    ExpectNoOverflowsOrUnderflows();
    HermeticAudioTest::TearDown();
  }

  std::pair<AudioRendererShim<SampleType>*, TypedFormat<SampleType>> CreateRenderer(
      int32_t frame_rate,
      fuchsia::media::AudioRenderUsage usage = fuchsia::media::AudioRenderUsage::MEDIA) {
    auto format = Format::Create<SampleType>(2, frame_rate).take_value();
    return std::make_pair(
        CreateAudioRenderer(format, PacketsToFrames(kNumPacketsInPayload, frame_rate), usage),
        format);
  }

  // All pipeline tests send batches of packets. This method returns the suggested size for
  // each batch. We want each batch to be large enough such that the output driver needs to
  // wake multiple times to mix the batch -- this ensures we're testing the timing paths in
  // the driver. We don't have direct access to the driver's timers, however, we know that
  // the driver must wake up at least once every MinLeadTime. Therefore, we return enough
  // packets to exceed one MinLeadTime.
  std::pair<int64_t, int64_t> NumPacketsAndFramesPerBatch(AudioRendererShim<SampleType>* renderer) {
    auto min_lead_time = renderer->min_lead_time();
    FX_CHECK(min_lead_time.get() > 0);
    // In exceptional cases, min_lead_time might be smaller than one packet.
    // Ensure we have at least a handful of packets.
    auto num_packets = std::max(5l, static_cast<int64_t>(min_lead_time / kPacketLength));
    FX_CHECK(num_packets < kNumPacketsInPayload);
    return std::make_pair(num_packets,
                          PacketsToFrames(num_packets, renderer->format().frames_per_second()));
  }

  VirtualOutput<SampleType>* output_ = nullptr;
};

using AudioRendererPipelineTestInt16 = AudioRendererPipelineTest<ASF::SIGNED_16>;
using AudioRendererPipelineTestFloat = AudioRendererPipelineTest<ASF::FLOAT>;

TEST_F(AudioRendererPipelineTestInt16, RenderSameFrameRate) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // The ring buffer should match the input buffer for the first num_packets.
  // The remaining bytes should be zeros.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, num_frames),
                      AudioBufferSlice(&input_buffer, 0, num_frames), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, num_frames, output_->frame_count()),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

TEST_F(AudioRendererPipelineTestInt16, RenderFasterFrameRate) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate * 2);
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  constexpr int16_t kSampleVal = 0xabc;
  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateConstantAudio(format, num_frames - kNumInitialSilentFrames, kSampleVal);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // Output is 2x slower, therefore has half as many frames.
  auto expected = GenerateSilentAudio(format, kNumInitialSilentFrames / 2);
  auto expected_signal =
      GenerateConstantAudio(format, (num_frames - kNumInitialSilentFrames) / 2, kSampleVal);
  expected.Append(&expected_signal);

  // The ring buffer should contain data followed by silence. Because this test uses
  // a different frame rate for the renderer vs the output device, we will use the
  // SincSampler, which emits the first frame one half "filter width" early then takes
  // one more half filter width to settle at the expected value.
  auto data_start = kNumInitialSilentFrames / 2 + kSincSamplerHalfFilterWidth;
  auto data_end = expected.NumFrames() - kSincSamplerHalfFilterWidth;
  auto silence_start = expected.NumFrames() + kSincSamplerHalfFilterWidth;
  auto silence_end = output_->frame_count() - kSincSamplerHalfFilterWidth;
  FX_LOGS(INFO) << "data_start " << data_start << ", data_end " << data_end << ", silence_start "
                << silence_start << ", silence_end " << silence_end;

  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, data_start, data_end),
                      AudioBufferSlice(&expected, data_start, data_end), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, silence_start, silence_end),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

TEST_F(AudioRendererPipelineTestInt16, RenderSlowerFrameRate) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate / 2);
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  constexpr int16_t kSampleVal = 0xabc;
  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateConstantAudio(format, num_frames - kNumInitialSilentFrames, kSampleVal);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // Output is 2x faster, therefore has twice as many frames.
  auto expected = GenerateSilentAudio(format, kNumInitialSilentFrames * 2);
  auto expected_signal =
      GenerateConstantAudio(format, (num_frames - kNumInitialSilentFrames) * 2, kSampleVal);
  expected.Append(&expected_signal);

  // The ring buffer should contain data followed by silence. Because this test uses
  // a different frame rate for the renderer vs the output device, we will use the
  // SincSampler, which takes one "filter width" to settle at the expected value.
  // We ignore that settling time.
  //
  // Also, since the renderer is 2x slower than the output, the filter is effectively
  // expanded to 2x larger in the output (plus one to round fractional frames).
  auto filter_half_width = 2 * kSincSamplerHalfFilterWidth + 1;
  auto data_start = kNumInitialSilentFrames * 2 + filter_half_width;
  auto data_end = expected.NumFrames() - filter_half_width;
  auto silence_start = expected.NumFrames() + filter_half_width;
  auto silence_end = output_->frame_count() - filter_half_width;

  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, data_start, data_end),
                      AudioBufferSlice(&expected, data_start, data_end), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, silence_start, silence_end),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

// Hardcoding this const -- it needs to match the value in AudioRenderer
constexpr zx::duration kPlayRampdownDuration = zx::msec(5);

TEST_F(AudioRendererPipelineTestInt16, PlayRampUp) {
  constexpr int64_t kPlayRampupFrames =
      kOutputFrameRate * kPlayRampdownDuration.get() / zx::sec(1).get();

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;

  constexpr int16_t kSampleVal = 0x7FF0;  // Very close to full-scale 16-bit
  auto input_buffer = GenerateConstantAudio(format, num_frames, kSampleVal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  // With current const values, NumPacketsAndFramesPerBatch provides 100 packets, but if this
  // changes we need at least enough packets for initial playback plus a complete rampdown
  ASSERT_GT(packets.size(), 2u);

  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, {packets[0], packets[1]});
  auto ring_buffer = output_->SnapshotRingBuffer();
  renderer->Pause(this);

  // The ring buffer should ramp from zero to the constant value, then hold steady for the rest.
  auto num_channels = ring_buffer.NumSamples() / ring_buffer.NumFrames();
  int64_t first_constant_frame = 0;

  // The first frame should be all zeroes
  for (auto chan = 0; chan < num_channels; ++chan) {
    EXPECT_EQ(ring_buffer.SampleAt(0, chan), 0) << "First frame should be zeroes";
  }

  // Starting at frame 1, values should successively ramp up (increase) to our const input value.
  for (auto frame = 1; frame < ring_buffer.NumFrames(); ++frame) {
    EXPECT_GT(ring_buffer.SampleAt(frame, 0), ring_buffer.SampleAt(frame - 1, 0))
        << "Frame values should monotonically ramp up (frarme " << frame << ")";
    for (auto chan = 1; chan < num_channels; ++chan) {
      // this sample should be more than this channel's in the previous frame
      EXPECT_EQ(ring_buffer.SampleAt(frame, 0), ring_buffer.SampleAt(frame, chan))
          << "Channel values should be identical (chan " << chan << ")";
    }
    if (ring_buffer.SampleAt(frame, 0) == kSampleVal) {
      first_constant_frame = frame;
      break;
    }
  }

  // The ramp duration is unknown to clients, but currently it is 5 msec so we use insider info.
  EXPECT_EQ(first_constant_frame, kPlayRampupFrames) << "Ramp-up duration was unexpected";

  // After the initial ramp-up, there should be at least one packet of the constant value.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = frames_per_packet;
  opts.test_label = "Constant value";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, first_constant_frame,
                                       first_constant_frame + frames_per_packet),
                      AudioBufferSlice(&input_buffer, first_constant_frame,
                                       first_constant_frame + frames_per_packet),
                      opts);
}

// Hardcoding this const -- it needs to match the value in AudioRenderer
constexpr zx::duration kPauseRampdownDuration = zx::msec(5);

TEST_F(AudioRendererPipelineTestInt16, PauseRampDown) {
  constexpr int64_t kPauseRampdownFrames =
      kOutputFrameRate * kPauseRampdownDuration.get() / zx::sec(1).get();

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  constexpr int16_t kSampleVal = 0x7FF0;  // Very close to full-scale 16-bit
  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateConstantAudio(format, num_frames - kNumInitialSilentFrames, kSampleVal);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  // With current const values, NumPacketsAndFramesPerBatch provides 100 packets, but if this
  // changes we need at least enough packets for initial playback plus a complete rampdown
  ASSERT_GT(packets.size(), 3u);

  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, {packets[0], packets[1]});
  auto [pause_ref_time, pause_media_time] = renderer->Pause(this);
  auto renderer_ref_time = renderer->ReferenceTimeFromMonotonicTime(zx::clock::get_monotonic());
  EXPECT_GT(renderer_ref_time.get(), pause_ref_time)
      << "Pause received pause_ref_time " << pause_ref_time
      << " in the future (now=" << renderer_ref_time.get() << ")";
  auto ring_buffer = output_->SnapshotRingBuffer();

  // The ring buffer should match the input buffer for the first 2 packets, then fade out.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = frames_per_packet;
  // First buffer should be silence actually.
  opts.test_label = "Initial silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, frames_per_packet),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);

  // Second buffer should be constant value.
  opts.test_label = "Constant value";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, frames_per_packet, 2 * frames_per_packet),
                      AudioBufferSlice(&signal, 0, frames_per_packet), opts);

  // We don't know exactly when the Pause took effect in the stream, so we search forward for the
  // first frame value that differs from the constant value.
  int64_t first_ramping_frame = 2 * frames_per_packet;
  int64_t first_zero_frame = 2 * frames_per_packet;
  auto num_channels = ring_buffer.NumSamples() / ring_buffer.NumFrames();

  for (auto frame = 2 * frames_per_packet; frame < ring_buffer.NumFrames(); ++frame) {
    for (auto chan = 1; chan < num_channels; ++chan) {
      EXPECT_EQ(ring_buffer.SampleAt(frame, chan), ring_buffer.SampleAt(frame, 0));
    }
    if (ring_buffer.SampleAt(frame, 0) != kSampleVal) {
      first_ramping_frame = frame;
      break;
    }
  }

  // Starting at first_ramping_frame, values should successively ramp down (decrease) to 0. This
  // ramp duration is unknown to clients, although currently it is 5 msec; we use insider info.
  for (auto frame = first_ramping_frame; frame < ring_buffer.NumFrames(); ++frame) {
    for (auto chan = 0; chan < num_channels; ++chan) {
      // this sample should be less than this channel's in the previous frame
      EXPECT_LT(ring_buffer.SampleAt(frame, chan), ring_buffer.SampleAt(frame - 1, chan))
          << "Frame values should monotonically ramp down";
    }
    if (ring_buffer.SampleAt(frame, 0) == 0) {
      first_zero_frame = frame;
      break;
    }
  }
  // From last constant-value frame to first zero-value frame, should be kPauseRampdownFrames.
  EXPECT_EQ(first_zero_frame - (first_ramping_frame - 1), kPauseRampdownFrames)
      << "Ramp-down duration was unexpected";

  for (auto frame = first_zero_frame; frame < ring_buffer.NumFrames(); ++frame) {
    for (auto chan = 0; chan < num_channels; ++chan) {
      // Every remaining sample should be zero.
      EXPECT_EQ(ring_buffer.SampleAt(frame, chan), 0);
    }
  }
}

TEST_F(AudioRendererPipelineTestInt16, DiscardDuringPlayback) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto kPacketFrames = PacketsToFrames(1, kOutputFrameRate);

  // Include a tolerance to account for scheduling latency.
  const auto min_lead_time_in_packets =
      renderer->min_lead_time() / kPacketLength + kToleranceInPackets;

  // This test writes to the ring buffer as follows:
  //
  // 1. The first step starts writing num_packets to the front of the ring buffer, but
  //    interrupts and discards after two packets have been written. Because of races,
  //    it's possible that more than two packets will have been written at the moment
  //    the remaining packets are discarded.
  //
  //     +---+---+ ...           +
  //     | P | P | maybe empty   |
  //     +---+---+ ...           +
  //
  //     ^..... num_packets .....^
  //
  // 2. The second step writes another num_packets, starting at least min_lead_time after
  //    the second packet:
  //
  //     +---+---+ ...           +---+ ...               +
  //     | P | P | maybe empty   | P | ...               |
  //     +---+---+ ...           +---+ ...               +
  //
  //             ^ min_lead_time ^
  //
  //     ^..... num_packets .....^..... num_packets .....^
  //
  // Note that 1 PTS == 1 frame.
  // To further simplify, all of the above sizes are integer numbers of packets.
  const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  // We insert one silent packet upfront, for initial gain ramp-up.
  const auto kNumInitialSilentFrames = frames_per_packet;

  // Load the renderer with lots of packets, but interrupt after two of them.
  const int64_t restart_packet = 2 + min_lead_time_in_packets;
  const int64_t restart_pts = restart_packet * kPacketFrames;

  auto first_input = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  first_input.Append(&signal);

  auto first_packets = renderer->AppendSlice(first_input, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, {first_packets[0], first_packets[1]});

  renderer->fidl()->DiscardAllPackets(AddCallback(
      "DiscardAllPackets", []() { FX_LOGS(DEBUG) << "DiscardAllPackets #1 complete"; }));
  ExpectCallbacks();

  // The entire first two packets must have been written. Subsequent packets may have been partially
  // written, depending on exactly when the DiscardAllPackets command is received. The remaining
  // bytes should be zeros.
  auto ring_buffer = output_->SnapshotRingBuffer();
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "first_input, first packet";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, 2 * kPacketFrames),
                      AudioBufferSlice(&first_input, 0, 2 * kPacketFrames), opts);
  opts.test_label = "first_input, third packet onwards";
  opts.partial = true;
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 2 * kPacketFrames, output_->frame_count()),
                      AudioBufferSlice(&first_input, 2 * kPacketFrames, output_->frame_count()),
                      opts);

  opts.partial = false;
  renderer->ClearPayload();

  // After interrupting the stream without stopping, now play another sequence of packets starting
  // at least "min_lead_time" after the last audio frame previously written to the ring buffer.
  // Between Left|Right, initial data values were odd|even; these are even|odd, for quick contrast
  // when visually inspecting the buffer.
  const int16_t restart_data_value = 0x4000;
  auto second_input = GenerateSequentialAudio(format, num_frames, restart_data_value);
  auto second_packets = renderer->AppendSlice(second_input, frames_per_packet, restart_pts);
  renderer->WaitForPackets(this, second_packets);

  // The ring buffer should contain first_input for 10ms (one packet), then partially-written data
  // followed by zeros until restart_pts, then second_input (num_packets), then the remaining bytes
  // should be zeros.
  ring_buffer = output_->SnapshotRingBuffer();

  opts.test_label = "first packet, after the second write";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, 2 * kPacketFrames),
                      AudioBufferSlice(&first_input, 0, 2 * kPacketFrames), opts);

  opts.test_label = "space between the first packet and second_input";
  opts.partial = true;
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 2 * kPacketFrames, restart_pts),
                      AudioBufferSlice(&first_input, 2 * kPacketFrames, restart_pts), opts);

  opts.test_label = "second_input";
  opts.partial = false;
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, restart_pts, restart_pts + num_frames),
                      AudioBufferSlice(&second_input, 0, num_frames), opts);

  opts.test_label = "silence after second_input";
  CompareAudioBuffers(
      AudioBufferSlice(&ring_buffer, restart_pts + num_frames, output_->frame_count()),
      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

// TODO(fxbug.dev/74985): Parameterize gainramp tests (volume-change, Play, Pause) for shared code.
TEST_F(AudioRendererPipelineTestInt16, RampOnGainChanges) {
  fuchsia::media::audio::VolumeControlPtr volume;
  audio_core_->BindUsageVolumeControl(
      fuchsia::media::Usage::WithRenderUsage(AudioRenderUsage::MEDIA), volume.NewRequest());
  volume->SetVolume(0.5);

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto num_packets = kNumPacketsInPayload;
  const auto num_frames = PacketsToFrames(num_packets, kOutputFrameRate);
  const auto frames_per_packet = num_frames / num_packets;

  const int16_t kSampleFullVolume = 0x0200;
  const int16_t kSampleHalfVolume = 0x0010;

  auto input_buffer = GenerateConstantAudio(format, num_frames, kSampleFullVolume);
  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  auto start_time = renderer->PlaySynchronized(this, output_, 0);

  // Wait until a few packets are rendered, then raise the volume to 1.0.
  auto start_delay = zx::time(start_time) - zx::clock::get_monotonic();
  RunLoopWithTimeout(start_delay + kPacketLength * (num_packets / 2));
  volume->SetVolume(1.0);

  // Now wait for all packets to be rendered.
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // The output should contain a sequence at half volume, followed by a ramp,
  // followed by a sequence at full volume. Verify that the length of the ramp
  // matches the expected ramp duration.
  int64_t start = ring_buffer.NumFrames() - 1;
  for (;; start--) {
    if (ring_buffer.SampleAt(start, 0) == kSampleHalfVolume) {
      break;
    }
    if (start == 0) {
      ADD_FAILURE() << "could not find half volume sample 0x" << std::hex << kSampleHalfVolume;
      ring_buffer.Display(0, 3 * kDebugFramesPerPacket);
      return;
    }
  }
  int64_t end = start + 1;
  for (;; end++) {
    if (ring_buffer.SampleAt(end, 0) == kSampleFullVolume) {
      break;
    }
    if (end == ring_buffer.NumFrames() - 1) {
      ADD_FAILURE() << "could not find full volume sample 0x" << std::hex << kSampleFullVolume
                    << " after frame " << std::dec << start;
      ring_buffer.Display(start, kDebugFramesPerPacket);
      return;
    }
  }

  // The exact length can be off by a fractional frame due to rounding.
  const auto ns_per_frame = format.frames_per_ns().Inverse();
  const auto dt = zx::nsec(ns_per_frame.Scale(end - start));
  const auto tol = zx::nsec(ns_per_frame.Scale(1));
  EXPECT_NEAR(kVolumeRampDuration.get(), dt.get(), tol.get())
      << "ramp has length " << (end - start) << " frames, from frame " << start << " to " << end;
}

// During playback, gain changes should not introduce high-frequency distortion.
TEST_F(AudioRendererPipelineTestFloat, NoDistortionOnGainChanges) {
  fuchsia::media::audio::VolumeControlPtr volume;
  audio_core_->BindUsageVolumeControl(
      fuchsia::media::Usage::WithRenderUsage(AudioRenderUsage::MEDIA), volume.NewRequest());
  volume->SetVolume(0.5);

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  const auto kPacketFrames = PacketsToFrames(1, kOutputFrameRate);
  int64_t num_frames =
      std::pow(2, std::floor(std::log2(PacketsToFrames(kNumPacketsInPayload, kOutputFrameRate))));

  // At 48kHz, this is 5.33ms per sinusoidal period. This is chosen intentionally to
  // (a) not align with volume updates, which happen every 10ms, and (b) include a
  // power-of-2 number of frames, to simplify the FFT comparison.
  const int64_t kFramesPerPeriod = 256;
  const int32_t freq = num_frames / kFramesPerPeriod;
  auto input_buffer = GenerateCosineAudio(format, num_frames, freq);
  auto packets = renderer->AppendSlice(input_buffer, kPacketFrames);
  auto start_time = renderer->PlaySynchronized(this, output_, 0);

  // Wait until the first packet will be rendered, then make a few gain toggles.
  RunLoopWithTimeout(zx::time(start_time) - zx::clock::get_monotonic());
  for (int64_t k = 0; k < num_frames / kPacketFrames; k++) {
    volume->SetVolume((k % 2) == 0 ? 1.0 : 0.5);
    RunLoopWithTimeout(kPacketLength);
  }

  // Now wait for all packets to be rendered.
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();
  auto output_buffer = AudioBufferSlice(&ring_buffer, 0, input_buffer.NumFrames()).GetChannel(0);

  // If we properly ramp gain changes, there should not be very much high-frequency noise.
  // For the purpose of this test, we'll define "high-frequency" to be anything at least 4
  // octaves above the base frequency.
  //
  // The precise amount of noise depends on exactly when the gain toggles are applied,
  // which is not deterministic. The noise signature also depends on the length and shape
  // of the gain ramp -- any intentional ramping change may break this test.
  //
  // As of early Aug 2020, typical noise_ratio values are:
  // * 0.05-0.07 without ramping
  // * 0.001-0.015 with ramping
  std::unordered_set<int32_t> highfreqs;
  for (int32_t f = freq << 4; f < output_buffer.NumFrames() / 2; f++) {
    highfreqs.insert(f);
  }
  auto result = MeasureAudioFreqs(AudioBufferSlice(&output_buffer), highfreqs);
  auto noise_ratio = result.total_magn_signal / result.total_magn_other;
  EXPECT_LT(noise_ratio, 0.02) << "\ntotal_magn_highfreq_noise = " << result.total_magn_signal
                               << "\ntotal_magn_other = " << result.total_magn_other;
}

class AudioRendererGainLimitsTest : public AudioRendererPipelineTestFloat {
 protected:
  // For these tests:
  //   min_gain_db = -20
  //   max_gain_db = -10
  static void SetUpTestSuite() {
    HermeticAudioTest::SetTestSuiteEnvironmentOptions(HermeticAudioEnvironment::Options{
        .audio_core_config_data_path = "/pkg/data/audio-core-config-with-gain-limits",
    });
  }

  // The test plays a sequence of constant values with amplitude 1.0. This output waveform's
  // amplitude will be adjusted by the specified stream and usage gains.
  struct TestCase {
    // Calls SetMute(true) if |input_stream_mute|, otherwise SetGain.
    float input_stream_gain_db = 0;
    bool input_stream_mute = false;
    // Calls SetMute(true) if |media_mute|, otherwise SetGain.
    float media_gain_db = 0;
    bool media_mute = false;
    float expected_output_sample = 1.0;
  };

  void Run(TestCase tc) {
    auto [renderer, format] = CreateRenderer(kOutputFrameRate);
    const auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
    const auto frames_per_packet = num_frames / num_packets;
    const auto kSilentPrefix = frames_per_packet;

    // Set stream gain/mute.
    fuchsia::media::audio::GainControlPtr gain_control;
    renderer->fidl()->BindGainControl(gain_control.NewRequest());
    AddErrorHandler(gain_control, "AudioRenderer::GainControl");
    if (tc.input_stream_mute) {
      gain_control->SetMute(true);
    } else {
      gain_control->SetGain(tc.input_stream_gain_db);
    }

    // Set usage gain/mute.
    if (tc.media_mute) {
      fuchsia::media::audio::VolumeControlPtr volume_control;
      audio_core_->BindUsageVolumeControl(
          fuchsia::media::Usage::WithRenderUsage(AudioRenderUsage::MEDIA),
          volume_control.NewRequest());
      volume_control->SetMute(true);
    } else {
      audio_core_->SetRenderUsageGain(AudioRenderUsage::MEDIA, tc.media_gain_db);
    }

    // Render.
    auto input_buffer = GenerateSilentAudio(format, kSilentPrefix);
    auto signal = GenerateConstantAudio(format, num_frames - kSilentPrefix, 1.0);
    input_buffer.Append(&signal);

    auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
    renderer->PlaySynchronized(this, output_, 0);
    renderer->WaitForPackets(this, packets);
    auto ring_buffer = output_->SnapshotRingBuffer();

    auto expected_output_buffer =
        GenerateConstantAudio(format, num_frames - kSilentPrefix, tc.expected_output_sample);

    CompareAudioBufferOptions opts;
    opts.num_frames_per_packet = kDebugFramesPerPacket;
    opts.test_label = "check initial silence";
    CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, kSilentPrefix),
                        AudioBufferSlice<ASF::FLOAT>(), opts);
    opts.test_label = "check data";
    CompareAudioBuffers(AudioBufferSlice(&ring_buffer, kSilentPrefix, num_frames - kSilentPrefix),
                        AudioBufferSlice(&expected_output_buffer, 0, num_frames - kSilentPrefix),
                        opts);
    opts.test_label = "check final silence";
    CompareAudioBuffers(AudioBufferSlice(&ring_buffer, num_frames, output_->frame_count()),
                        AudioBufferSlice<ASF::FLOAT>(), opts);
  }
};

TEST_F(AudioRendererGainLimitsTest, StreamGainRespectsMinGain) {
  Run({
      .input_stream_gain_db = -30,
      .expected_output_sample = Gain::DbToScale(-20),
  });
}

TEST_F(AudioRendererGainLimitsTest, StreamGainsRespectsMaxGain) {
  Run({
      .input_stream_gain_db = -1,
      .expected_output_sample = Gain::DbToScale(-10),
  });
}

TEST_F(AudioRendererGainLimitsTest, StreamGainInRange) {
  Run({
      .input_stream_gain_db = -15,
      .expected_output_sample = Gain::DbToScale(-15),
  });
}

TEST_F(AudioRendererGainLimitsTest, UsageGainRespectsMinGain) {
  Run({
      .media_gain_db = -30,
      .expected_output_sample = Gain::DbToScale(-20),
  });
}

TEST_F(AudioRendererGainLimitsTest, UsageGainsRespectsMaxGain) {
  Run({
      .media_gain_db = -1,
      .expected_output_sample = Gain::DbToScale(-10),
  });
}

TEST_F(AudioRendererGainLimitsTest, UsageGainInRange) {
  Run({
      .media_gain_db = -15,
      .expected_output_sample = Gain::DbToScale(-15),
  });
}

TEST_F(AudioRendererGainLimitsTest, KeepStreamMuteGain) {
  Run({
      .input_stream_gain_db = fuchsia::media::audio::MUTED_GAIN_DB,
      .expected_output_sample = 0,
  });
}

TEST_F(AudioRendererGainLimitsTest, KeepStreamMute) {
  Run({
      .input_stream_mute = true,
      .expected_output_sample = 0,
  });
}

TEST_F(AudioRendererGainLimitsTest, KeepUsageMuteGain) {
  Run({
      .media_gain_db = fuchsia::media::audio::MUTED_GAIN_DB,
      .expected_output_sample = 0,
  });
}

TEST_F(AudioRendererGainLimitsTest, KeepUsageMute) {
  Run({
      .media_mute = true,
      .expected_output_sample = 0,
  });
}

class AudioRendererPipelineUnderflowTest : public HermeticAudioTest {
 protected:
  static constexpr int32_t kFrameRate = 48000;
  static constexpr auto kPacketFrames = kFrameRate / 100;

  static void SetUpTestSuite() {
    HermeticAudioTest::SetTestSuiteEnvironmentOptions(HermeticAudioEnvironment::Options{
        .audio_core_base_url = "fuchsia-pkg://fuchsia.com/audio-core-with-test-effects",
        .audio_core_config_data_path = "/pkg/data/audio-core-config-with-sleeper-filter",
    });
  }

  AudioRendererPipelineUnderflowTest()
      : format_(Format::Create<ASF::SIGNED_16>(2, kFrameRate).value()) {}

  void SetUp() override {
    HermeticAudioTest::SetUp();
    output_ = CreateOutput({{0xff, 0x00}}, format_, kFrameRate);
    renderer_ = CreateAudioRenderer(format_, kFrameRate);
  }

  const TypedFormat<ASF::SIGNED_16> format_;
  VirtualOutput<ASF::SIGNED_16>* output_ = nullptr;
  AudioRendererShim<ASF::SIGNED_16>* renderer_ = nullptr;
};

// Validate that a slow effects pipeline registers an underflow.
TEST_F(AudioRendererPipelineUnderflowTest, HasUnderflow) {
  // Inject one packet and wait for it to be rendered.
  auto input_buffer = GenerateSequentialAudio(format_, kPacketFrames);
  auto packets = renderer_->AppendSlice(input_buffer, kPacketFrames);
  renderer_->PlaySynchronized(this, output_, 0);
  renderer_->WaitForPackets(this, packets);

  // Wait an extra 20ms to account for the sleeper filter's delay.
  RunLoopWithTimeout(zx::msec(20));

  // Expect an underflow.
  ExpectInspectMetrics(output_, {
                                    .children =
                                        {
                                            {"pipeline underflows", {.nonzero_uints = {"count"}}},
                                        },
                                });
}

class AudioRendererPipelineEffectsTest : public AudioRendererPipelineTestInt16 {
 protected:
  // Matches the value in audio_core_config_with_inversion_filter.json
  static constexpr const char* kInverterEffectName = "inverter";

  static void SetUpTestSuite() {
    HermeticAudioTest::SetTestSuiteEnvironmentOptions(HermeticAudioEnvironment::Options{
        .audio_core_base_url = "fuchsia-pkg://fuchsia.com/audio-core-with-test-effects",
        .audio_core_config_data_path = "/pkg/data/audio-core-config-with-inversion-filter",
    });
  }

  void SetUp() override {
    AudioRendererPipelineTestInt16::SetUp();
    environment()->ConnectToService(effects_controller_.NewRequest());
  }

  void RunInversionFilter(AudioBuffer<ASF::SIGNED_16>* audio_buffer_ptr) {
    auto& samples = audio_buffer_ptr->samples();
    for (auto sample = 0u; sample < samples.size(); sample++) {
      samples[sample] = -samples[sample];
    }
  }

  fuchsia::media::audio::EffectsControllerSyncPtr effects_controller_;
};

// Validate that the effects package is loaded and that it processes the input.
TEST_F(AudioRendererPipelineEffectsTest, RenderWithEffects) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // Simulate running the effect on the input buffer.
  RunInversionFilter(&input_buffer);

  // The ring buffer should match the transformed input buffer for the first num_packets.
  // The remaining bytes should be zeros.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, num_frames),
                      AudioBufferSlice(&input_buffer, 0, num_frames), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, num_frames, output_->frame_count()),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

TEST_F(AudioRendererPipelineEffectsTest, EffectsControllerEffectDoesNotExist) {
  fuchsia::media::audio::EffectsController_UpdateEffect_Result result;
  zx_status_t status = effects_controller_->UpdateEffect("invalid_effect_name", "disable", &result);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(result.is_err());
  EXPECT_EQ(result.err(), fuchsia::media::audio::UpdateEffectError::NOT_FOUND);
}

TEST_F(AudioRendererPipelineEffectsTest, EffectsControllerInvalidConfig) {
  fuchsia::media::audio::EffectsController_UpdateEffect_Result result;
  zx_status_t status =
      effects_controller_->UpdateEffect(kInverterEffectName, "invalid config string", &result);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(result.is_err());
  EXPECT_EQ(result.err(), fuchsia::media::audio::UpdateEffectError::INVALID_CONFIG);
}

// Similar to RenderWithEffects, except we send a message to the effect to disable processing.
TEST_F(AudioRendererPipelineEffectsTest, EffectsControllerUpdateEffect) {
  // Disable the inverter; frames should be unmodified.
  fuchsia::media::audio::EffectsController_UpdateEffect_Result result;
  zx_status_t status = effects_controller_->UpdateEffect(kInverterEffectName, "disable", &result);
  EXPECT_EQ(status, ZX_OK);
  EXPECT_TRUE(result.is_response());

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  auto [num_packets, num_frames] = NumPacketsAndFramesPerBatch(renderer);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  input_buffer.Append(&signal);

  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // The ring buffer should match the input buffer for the first num_packets. The remaining bytes
  // should be zeros.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, num_frames),
                      AudioBufferSlice(&input_buffer, 0, num_frames), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, num_frames, output_->frame_count()),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

class AudioRendererPipelineTuningTest : public AudioRendererPipelineTestInt16 {
 protected:
  // Matches the value in audio_core_config_with_inversion_filter.json
  static constexpr const char* kInverterEffectName = "inverter";

  static void SetUpTestSuite() {
    HermeticAudioTest::SetTestSuiteEnvironmentOptions(HermeticAudioEnvironment::Options{
        .audio_core_base_url = "fuchsia-pkg://fuchsia.com/audio-core-with-test-effects",
        .audio_core_config_data_path = "/pkg/data/audio-core-config-with-inversion-filter",
    });
  }

  void SetUp() override {
    AudioRendererPipelineTestInt16::SetUp();
    environment()->ConnectToService(audio_tuner_.NewRequest());
  }

  void RunInversionFilter(AudioBuffer<ASF::SIGNED_16>* audio_buffer_ptr) {
    auto& samples = audio_buffer_ptr->samples();
    for (auto sample = 0u; sample < samples.size(); sample++) {
      samples[sample] = -samples[sample];
    }
  }

  fuchsia::media::tuning::AudioTunerPtr audio_tuner_;
};

// Verify the correct output is received before and after update of the OutputPipeline.
//
// AudioCore is launched with a default profile containing an inversion_filter effect; a renderer
// plays a packet, and the output is verified as inverted. Then, the AudioTuner service is used to
// update the OutputPipeline with a PipelineConfig containing a disabled inversion_filter effect. A
// second packet is played, and the output is verified as having no effects applied.
TEST_F(AudioRendererPipelineTuningTest, CorrectStreamOutputUponUpdatedPipeline) {
  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  auto num_packets = 1;
  auto num_frames = PacketsToFrames(num_packets, kOutputFrameRate);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  // Initiate stream with first packets and send through default OutputPipeline, which has an
  // inversion_filter effect enabled.
  auto first_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  first_buffer.Append(&signal);

  auto first_packets = renderer->AppendSlice(first_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, first_packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // Prepare first buffer for comparison to expected ring buffer.
  RunInversionFilter(&first_buffer);

  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "default config, first packet";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, num_frames),
                      AudioBufferSlice(&first_buffer), opts);

  // Clear payload to avoid overlap of values from old OutputPipeline ringout with values from new
  // OutputPipeline.
  renderer->ClearPayload();

  // Setup new output pipeline details.
  auto device_id = AudioDevice::UniqueIdToString({{0xff, 0x00}});
  PipelineConfig::MixGroup root{.name = "linearize",
                                .input_streams =
                                    {
                                        RenderUsage::MEDIA,
                                        RenderUsage::SYSTEM_AGENT,
                                        RenderUsage::INTERRUPTION,
                                        RenderUsage::COMMUNICATION,
                                    },
                                .effects = {
                                    {
                                        .lib_name = "inversion_filter.so",
                                        .effect_name = "inversion_filter",
                                        .instance_name = "inverter",
                                        .effect_config = "disable",
                                    },
                                }};
  auto pipeline_config = PipelineConfig(root);
  auto volume_curve = VolumeCurve::DefaultForMinGain(VolumeCurve::kDefaultGainForMinVolume);
  auto device_profile_with_inversion_effect =
      ToAudioDeviceTuningProfile(pipeline_config, volume_curve);

  // Update PipelineConfig through AudioTuner service.
  audio_tuner_->SetAudioDeviceProfile(
      device_id, std::move(device_profile_with_inversion_effect),
      AddCallback("SetAudioDeviceProfile", [](zx_status_t status) { EXPECT_EQ(status, ZX_OK); }));

  ExpectCallbacks();

  // Send second set of packets through new OutputPipeline (with inversion effect disabled); play
  // packets at least "min_lead_time" after the last audio frame previously written to the ring
  // buffer. Include a tolerance to account for scheduling delay.
  const auto min_lead_time_in_packets =
      renderer->min_lead_time() / kPacketLength + kToleranceInPackets;
  const int64_t restart_packet = 2 + min_lead_time_in_packets;
  const int64_t restart_pts = PacketsToFrames(restart_packet, kOutputFrameRate);

  auto second_buffer = GenerateSequentialAudio(format, num_frames);
  auto second_packets = renderer->AppendSlice(second_buffer, frames_per_packet, restart_pts);
  renderer->WaitForPackets(this, second_packets);
  ring_buffer = output_->SnapshotRingBuffer();

  // Verify the remaining packets have gone through the updated OutputPipeline and thus been
  // unmodified, due to the inversion_filter being disabled in the new configuration.
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "updated config, remaining packets";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, restart_pts, restart_pts + num_frames),
                      AudioBufferSlice(&second_buffer), opts);
}

// Verify the correct output is received after update of the specified effect config.
//
// AudioCore is launched with a default profile containing an inversion_filter effect. The
// AudioTuner service is used to update a specified effect instance's effect configuration, which
// disables the inversion_filter effect present in the default profile. A packet is played, and the
// output is verified as having the inversion_filter effect disabled (no effects applied).
TEST_F(AudioRendererPipelineTuningTest, AudioTunerUpdateEffect) {
  // Disable the inverter; frames should be unmodified.
  auto device_id = AudioDevice::UniqueIdToString({{0xff, 0x00}});
  fuchsia::media::tuning::AudioEffectConfig updated_effect;
  updated_effect.set_instance_name(kInverterEffectName);
  updated_effect.set_configuration("disable");
  audio_tuner_->SetAudioEffectConfig(
      device_id, std::move(updated_effect),
      AddCallback("SetAudioEffectConfig", [](zx_status_t status) { EXPECT_EQ(status, ZX_OK); }));

  ExpectCallbacks();

  auto [renderer, format] = CreateRenderer(kOutputFrameRate);
  auto min_lead_time = renderer->min_lead_time();
  auto num_packets = min_lead_time / kPacketLength;
  auto num_frames = PacketsToFrames(num_packets, kOutputFrameRate);
  const auto frames_per_packet = num_frames / num_packets;
  const auto kNumInitialSilentFrames = frames_per_packet;

  auto input_buffer = GenerateSilentAudio(format, kNumInitialSilentFrames);
  auto signal = GenerateSequentialAudio(format, num_frames - kNumInitialSilentFrames);
  input_buffer.Append(&signal);
  auto packets = renderer->AppendSlice(input_buffer, frames_per_packet);
  renderer->PlaySynchronized(this, output_, 0);
  renderer->WaitForPackets(this, packets);
  auto ring_buffer = output_->SnapshotRingBuffer();

  // Ring buffer should match input buffer, thru num_packets. The remainder should be zeros.
  CompareAudioBufferOptions opts;
  opts.num_frames_per_packet = kDebugFramesPerPacket;
  opts.test_label = "check data";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, 0, num_frames),
                      AudioBufferSlice(&input_buffer, 0, num_frames), opts);
  opts.test_label = "check silence";
  CompareAudioBuffers(AudioBufferSlice(&ring_buffer, num_frames, output_->frame_count()),
                      AudioBufferSlice<ASF::SIGNED_16>(), opts);
}

// /// Overall, need to add tests to validate various Renderer pipeline aspects
// TODO(mpuryear): validate the combinations of NO_TIMESTAMP (Play ref_time,
//     Play media_time, packet PTS)
// TODO(mpuryear): validate channelization (future)
// TODO(mpuryear): validate sample format
// TODO(mpuryear): validate various permutations of PtsUnits. Ref clocks?
// TODO(mpuryear): handle EndOfStream?
// TODO(mpuryear): test >1 payload buffer
// TODO(mpuryear): test late packets (no timestamps), gap-then-signal at driver.
//     Should include various permutations of MinLeadTime, ContinuityThreshold
// TODO(mpuryear): test packets with timestamps already played -- expect
//     truncated-signal at driver
// TODO(mpuryear): test packets with timestamps too late -- expect Renderer
//     gap-then-truncated-signal at driver
// TODO(mpuryear): test that no data is lost when Renderer Play-Pause-Play

}  // namespace media::audio::test
