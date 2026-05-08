#!/usr/bin/env bash
# Builds the audiotee Swift CLI from the vendored upstream and copies the
# resulting binary into polyglot-app/src-tauri/binaries/ where Tauri expects
# its sidecar.
#
# audiotee's normal SPM build (`swift build`) doesn't work under
# CommandLineTools-only Swift installations (the manifest API can't be
# linked). We work around that by compiling all the .swift files as a
# single module via `swiftc`. That also requires stripping the
# `import AudioTeeCore` lines from the CLI files since they would have
# crossed module boundaries under SPM.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$ROOT/vendor/audiotee"
TARGET="$ROOT/polyglot-app/src-tauri/binaries/audiotee-aarch64-apple-darwin"

if [ ! -d "$VENDOR" ]; then
  echo "Cloning audiotee..."
  mkdir -p "$ROOT/vendor"
  git clone --depth 1 https://github.com/makeusabrew/audiotee.git "$VENDOR"
fi

cd "$VENDOR"
git checkout -- . 2>/dev/null || true
sed -i '' '/^import AudioTeeCore$/d' Sources/AudioTeeCLI/*.swift

# Patch 1: map `--mute` to CATapMuteBehavior.mutedWhenTapped (capture + mute
# playback) instead of .muted (full mute, no capture).
cat > Sources/AudioTeeCore/Core/TapMuteBehavior.swift <<'SWIFT'
import CoreAudio

public enum TapMuteBehavior: String, CaseIterable {
  case unmuted = "unmuted"
  case muted = "muted"

  public var description: String {
    switch self {
    case .unmuted:
      return "Don't mute processes (default)"
    case .muted:
      return "Mute tapped processes' speaker output but still capture audio"
    }
  }

  public var coreAudioValue: CATapMuteBehavior {
    switch self {
    case .unmuted:
      return .unmuted
    case .muted:
      return .mutedWhenTapped
    }
  }
}
SWIFT

# Patch 2: fix the aggregate-device-creation race that ships in upstream audiotee.
# The tap MUST be included in the aggregate device's creation dictionary via
# kAudioAggregateDeviceTapListKey. Setting it post-hoc with AudioObjectSetPropertyData
# silently produces zero PCM on macOS 15+. This matches insidegui/AudioCap's pattern
# and audiotee's open PR #14.
cat > Sources/AudioTeeCore/Core/AudioTapManager.swift <<'SWIFT'
import AVFoundation
import AudioToolbox
import CoreAudio
import Foundation

public class AudioTapManager {
  private var tapID: AudioObjectID?
  private var deviceID: AudioObjectID?

  public init() {}

  deinit {
    AudioTeeLogging.logger.debug("Cleaning up audio tap manager")
    if let tapID = tapID {
      AudioHardwareDestroyProcessTap(tapID)
      self.tapID = nil
    }
    if let deviceID = deviceID {
      AudioHardwareDestroyAggregateDevice(deviceID)
      self.deviceID = nil
    }
  }

  /// Sets up the audio tap and aggregate device.
  public func setupAudioTap(with config: TapConfiguration) throws {
    AudioTeeLogging.logger.debug("Setting up audio tap manager")
    let (createdTapID, tapUUID) = try createSystemAudioTap(with: config)
    tapID = createdTapID
    deviceID = try createAggregateDevice(tapUUID: tapUUID)
    guard tapID != nil, deviceID != nil else { throw AudioTeeError.setupFailed }
    AudioTeeLogging.logger.debug("Audio tap manager setup complete")
  }

  public func getDeviceID() -> AudioObjectID? { return deviceID }

  private func createSystemAudioTap(with config: TapConfiguration) throws -> (AudioObjectID, String) {
    AudioTeeLogging.logger.debug("Creating tap description")
    let description = CATapDescription()
    description.name = "audiotee-tap"
    description.processes = try translatePIDsToProcessObjects(config.processes)
    description.isPrivate = true
    description.muteBehavior = config.muteBehavior.coreAudioValue
    description.isMixdown = true
    description.isMono = config.isMono
    description.isExclusive = config.isExclusive
    description.deviceUID = nil
    description.stream = 0
    description.uuid = UUID()
    let tapUUID = description.uuid.uuidString

    AudioTeeLogging.logger.debug(
      "Tap description configured",
      context: [
        "name": description.name,
        "processes": String(describing: config.processes),
        "mute": String(describing: description.muteBehavior),
        "mono": String(description.isMono),
        "exclusive": String(description.isExclusive),
        "uuid": tapUUID,
      ])

    AudioTeeLogging.logger.debug("Creating tap")
    var tapID = AudioObjectID(kAudioObjectUnknown)
    let status = AudioHardwareCreateProcessTap(description, &tapID)
    AudioTeeLogging.logger.debug(
      "AudioHardwareCreateProcessTap completed", context: ["status": String(status)])
    guard status == kAudioHardwareNoError else {
      AudioTeeLogging.logger.error(
        "Failed to create audio tap", context: ["status": String(status)])
      throw AudioTeeError.tapCreationFailed(status)
    }

    var propertyAddress = getPropertyAddress(selector: kAudioTapPropertyFormat)
    var propertySize = UInt32(MemoryLayout<AudioStreamBasicDescription>.stride)
    var streamDescription = AudioStreamBasicDescription()
    let formatStatus = AudioObjectGetPropertyData(
      tapID, &propertyAddress, 0, nil, &propertySize, &streamDescription)
    if formatStatus == noErr {
      AudioTeeLogging.logger.debug(
        "Tap format retrieved",
        context: [
          "channels": String(streamDescription.mChannelsPerFrame),
          "sample_rate": String(Int(streamDescription.mSampleRate)),
        ])
    }

    return (tapID, tapUUID)
  }

  private func createAggregateDevice(tapUUID: String) throws -> AudioObjectID {
    let uid = UUID().uuidString
    let tapList: [[String: Any]] = [[
      kAudioSubTapUIDKey: tapUUID,
      kAudioSubTapDriftCompensationKey: true,
    ]]
    let description: [String: Any] = [
      kAudioAggregateDeviceNameKey: "audiotee-aggregate-device",
      kAudioAggregateDeviceUIDKey: uid,
      kAudioAggregateDeviceTapListKey: tapList,
      kAudioAggregateDeviceTapAutoStartKey: true,
      kAudioAggregateDeviceIsPrivateKey: true,
      kAudioAggregateDeviceIsStackedKey: false,
    ]
    var deviceID: AudioObjectID = 0
    let status = AudioHardwareCreateAggregateDevice(description as CFDictionary, &deviceID)
    guard status == kAudioHardwareNoError else {
      AudioTeeLogging.logger.error(
        "Failed to create aggregate device", context: ["status": String(status)])
      throw AudioTeeError.aggregateDeviceCreationFailed(status)
    }
    return deviceID
  }
}
SWIFT

xcrun swiftc -O \
  -framework AVFoundation \
  -framework CoreAudio \
  -framework AudioToolbox \
  -framework Foundation \
  Sources/AudioTeeCore/Core/*.swift \
  Sources/AudioTeeCore/Output/*.swift \
  Sources/AudioTeeCore/Utils/*.swift \
  Sources/AudioTeeCLI/*.swift \
  -o audiotee

mkdir -p "$(dirname "$TARGET")"
cp audiotee "$TARGET"
echo "Built: $TARGET ($(stat -f '%z' "$TARGET") bytes)"
