//! Negotiated streaming experiment types and pure policy functions.
//!
//! This module intentionally contains no I/O.  The server, agent and browser use
//! the same serialized contracts while policy and controller decisions remain
//! deterministic and unit-testable.

use serde::{Deserialize, Serialize};

pub const EXPERIMENT_SCHEMA_VERSION: u16 = 1;
pub const MAX_CODEC_CANDIDATES: usize = 16;
pub const MAX_HEADER_VERSIONS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    #[default]
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodecProfile {
    Auto,
    #[default]
    Main,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MediaTransport {
    #[default]
    Websocket,
    WebtransportDatagram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BrowserPipeline {
    #[default]
    MainThread,
    WorkerTransferredBuffers,
    WorkerOwnedTransport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DependencyClass {
    Key,
    #[default]
    Reference,
    Disposable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CongestionMode {
    #[default]
    Off,
    Observe,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    #[default]
    FixedRate,
    DuplicateSuppression,
    XDamage,
    XDamageRegions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecCapability {
    pub codec: Codec,
    /// Exact WebCodecs codec string that was probed (for example avc1.4d0033).
    pub codec_string: String,
    #[serde(default)]
    pub supported: bool,
    #[serde(default)]
    pub hardware_acceleration: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub protocol_versions: Vec<u8>,
    #[serde(default)]
    pub frame_header_versions: Vec<u8>,
    #[serde(default)]
    pub codecs: Vec<CodecCapability>,
    #[serde(default)]
    pub webcodecs_worker: bool,
    #[serde(default)]
    pub offscreen_canvas: bool,
    #[serde(default)]
    pub webtransport: bool,
    #[serde(default)]
    pub webtransport_datagrams: bool,
    pub max_decode_width: Option<u32>,
    pub max_decode_height: Option<u32>,
    pub max_decode_pixels: Option<u64>,
}

impl ClientCapabilities {
    pub fn sanitized(mut self) -> Self {
        self.protocol_versions.truncate(MAX_HEADER_VERSIONS);
        self.frame_header_versions.truncate(MAX_HEADER_VERSIONS);
        self.codecs.truncate(MAX_CODEC_CANDIDATES);
        for candidate in &mut self.codecs {
            candidate.codec_string.truncate(64);
        }
        self.max_decode_width = self.max_decode_width.map(|v| v.clamp(320, 16_384));
        self.max_decode_height = self.max_decode_height.map(|v| v.clamp(240, 16_384));
        self.max_decode_pixels = self.max_decode_pixels.map(|v| v.clamp(76_800, 134_217_728));
        self
    }

    pub fn supports_header(&self, version: u8) -> bool {
        self.frame_header_versions.contains(&version)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizingIntent {
    pub css_width: u32,
    pub css_height: u32,
    pub device_pixel_ratio: f64,
    #[serde(default = "one")]
    pub render_scale: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SizingLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_pixels: u64,
    pub max_dpr: f64,
    pub alignment: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveSizing {
    pub css_width: u32,
    pub css_height: u32,
    pub encoded_width: u32,
    pub encoded_height: u32,
    pub requested_dpr: f64,
    pub effective_dpr_x: f64,
    pub effective_dpr_y: f64,
    pub render_scale: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limiting_reasons: Vec<String>,
}

/// Select the largest aligned size that preserves aspect ratio and satisfies
/// every dimension, pixel and DPR bound. Invalid browser numbers are replaced
/// with conservative values rather than participating in allocation math.
pub fn compute_effective_sizing(
    intent: SizingIntent,
    limits: SizingLimits,
    caps: Option<&ClientCapabilities>,
) -> EffectiveSizing {
    let css_width = intent.css_width.clamp(320, 16_384);
    let css_height = intent.css_height.clamp(240, 16_384);
    let requested_dpr = if intent.device_pixel_ratio.is_finite() {
        intent.device_pixel_ratio.clamp(0.5, 4.0)
    } else {
        1.0
    };
    let render_scale = if intent.render_scale.is_finite() {
        intent.render_scale.clamp(0.25, 1.0)
    } else {
        1.0
    };
    let max_dpr = if limits.max_dpr.is_finite() {
        limits.max_dpr.clamp(0.5, 4.0)
    } else {
        1.0
    };
    let mut reasons = Vec::new();
    let dpr = requested_dpr.min(max_dpr);
    if dpr < requested_dpr {
        reasons.push("server_dpr_cap".to_string());
    }

    let desired_width = css_width as f64 * dpr * render_scale;
    let desired_height = css_height as f64 * dpr * render_scale;
    if render_scale < 1.0 {
        reasons.push("render_scale".to_string());
    }

    let server_max_width = if limits.max_width == 0 {
        16_384
    } else {
        limits.max_width
    };
    let server_max_height = if limits.max_height == 0 {
        16_384
    } else {
        limits.max_height
    };
    let client_max_width = caps.and_then(|c| c.max_decode_width).unwrap_or(16_384);
    let client_max_height = caps.and_then(|c| c.max_decode_height).unwrap_or(16_384);
    let max_width = server_max_width.min(client_max_width) as f64;
    let max_height = server_max_height.min(client_max_height) as f64;
    let server_pixels = if limits.max_pixels == 0 {
        u64::MAX
    } else {
        limits.max_pixels
    };
    let client_pixels = caps.and_then(|c| c.max_decode_pixels).unwrap_or(u64::MAX);
    let max_pixels = server_pixels.min(client_pixels) as f64;

    let mut factor = 1.0_f64;
    if desired_width > max_width {
        factor = factor.min(max_width / desired_width);
        reasons.push(
            if client_max_width < server_max_width {
                "client_width_cap"
            } else {
                "server_width_cap"
            }
            .to_string(),
        );
    }
    if desired_height > max_height {
        factor = factor.min(max_height / desired_height);
        reasons.push(
            if client_max_height < server_max_height {
                "client_height_cap"
            } else {
                "server_height_cap"
            }
            .to_string(),
        );
    }
    let desired_pixels = desired_width * desired_height;
    if desired_pixels > max_pixels {
        factor = factor.min((max_pixels / desired_pixels).sqrt());
        reasons.push(
            if client_pixels < server_pixels {
                "client_pixel_cap"
            } else {
                "server_pixel_cap"
            }
            .to_string(),
        );
    }

    let alignment = limits.alignment.max(2);
    let align_down = |value: f64| -> u32 {
        let raw = value.floor().max(alignment as f64) as u32;
        (raw / alignment) * alignment
    };
    let encoded_width = align_down(desired_width * factor);
    let encoded_height = align_down(desired_height * factor);
    if encoded_width as f64 != (desired_width * factor).floor()
        || encoded_height as f64 != (desired_height * factor).floor()
    {
        reasons.push("codec_alignment".to_string());
    }
    reasons.dedup();

    EffectiveSizing {
        css_width,
        css_height,
        encoded_width,
        encoded_height,
        requested_dpr,
        effective_dpr_x: encoded_width as f64 / css_width as f64,
        effective_dpr_y: encoded_height as f64 / css_height as f64,
        render_scale,
        limiting_reasons: reasons,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamDescriptor {
    #[serde(default = "default_schema")]
    pub schema_version: u16,
    #[serde(default)]
    pub stream_generation: u32,
    #[serde(default)]
    pub codec: Codec,
    #[serde(default)]
    pub profile: CodecProfile,
    pub level: Option<String>,
    pub codec_string: Option<String>,
    #[serde(default = "default_encoder")]
    pub encoder: String,
    #[serde(default)]
    pub media_transport: MediaTransport,
    #[serde(default = "default_control_transport")]
    pub control_transport: String,
    #[serde(default)]
    pub browser_pipeline: BrowserPipeline,
    pub sizing: EffectiveSizing,
    pub fps_target: u32,
    pub bitrate_kbps: u32,
    #[serde(default = "one")]
    pub render_scale: f64,
    #[serde(default = "default_treatment")]
    pub treatment_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_reasons: Vec<String>,
    #[serde(default = "default_header_version")]
    pub frame_header_version: u8,
}

fn default_schema() -> u16 {
    EXPERIMENT_SCHEMA_VERSION
}
fn default_encoder() -> String {
    "auto".to_string()
}
fn default_control_transport() -> String {
    "wss".to_string()
}
fn default_treatment() -> String {
    "baseline".to_string()
}
fn default_header_version() -> u8 {
    1
}

impl Default for EffectiveSizing {
    fn default() -> Self {
        Self {
            css_width: 1920,
            css_height: 1080,
            encoded_width: 1920,
            encoded_height: 1080,
            requested_dpr: 1.0,
            effective_dpr_x: 1.0,
            effective_dpr_y: 1.0,
            render_scale: 1.0,
            limiting_reasons: Vec::new(),
        }
    }
}

impl Default for StreamDescriptor {
    fn default() -> Self {
        Self {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            stream_generation: 0,
            codec: Codec::H264,
            profile: CodecProfile::Main,
            level: None,
            codec_string: Some("avc1.4d0033".to_string()),
            encoder: default_encoder(),
            media_transport: MediaTransport::Websocket,
            control_transport: default_control_transport(),
            browser_pipeline: BrowserPipeline::MainThread,
            sizing: EffectiveSizing::default(),
            fps_target: 60,
            bitrate_kbps: 15_000,
            render_scale: 1.0,
            treatment_id: default_treatment(),
            fallback_reasons: Vec::new(),
            frame_header_version: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentManifest {
    pub schema_version: u16,
    pub treatment_id: String,
    pub beam_version: String,
    pub beam_commit: Option<String>,
    pub workload_id: Option<String>,
    pub network_profile: Option<String>,
    pub stream: StreamDescriptor,
}

impl ExperimentManifest {
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        // Struct field order is stable and maps are intentionally absent.
        serde_json::to_string(self)
    }
}

/// NTP four-timestamp estimate. All timestamps are microseconds on their own
/// monotonic clocks. Offset is remote minus local.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockEstimate {
    pub offset_us: i64,
    pub round_trip_us: u64,
    pub uncertainty_us: u64,
}

pub fn estimate_clock_offset(
    t0_local: u64,
    t1_remote: u64,
    t2_remote: u64,
    t3_local: u64,
) -> Option<ClockEstimate> {
    if t3_local < t0_local || t2_remote < t1_remote {
        return None;
    }
    let network_rtt = (t3_local - t0_local).saturating_sub(t2_remote - t1_remote);
    let offset =
        ((t1_remote as i128 - t0_local as i128) + (t2_remote as i128 - t3_local as i128)) / 2;
    Some(ClockEstimate {
        offset_us: offset.clamp(i64::MIN as i128, i64::MAX as i128) as i64,
        round_trip_us: network_rtt,
        uncertainty_us: (network_rtt / 2).max(1),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    AwaitingKey,
    Streaming,
    AwaitingRecovery,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecision {
    Admit,
    DropDelta,
    AdmitAndRecover,
    RejectOldGeneration,
}

#[derive(Debug, Clone, Copy)]
pub struct RecoveryMachine {
    pub generation: u32,
    pub state: RecoveryState,
    pub last_sequence: Option<u64>,
}

impl RecoveryMachine {
    pub fn new(generation: u32) -> Self {
        Self {
            generation,
            state: RecoveryState::AwaitingKey,
            last_sequence: None,
        }
    }

    pub fn reset(&mut self, generation: u32) {
        self.generation = generation;
        self.state = RecoveryState::AwaitingKey;
        self.last_sequence = None;
    }

    pub fn admit(
        &mut self,
        generation: u32,
        sequence: u64,
        dependency: DependencyClass,
    ) -> FrameDecision {
        if generation != self.generation {
            return FrameDecision::RejectOldGeneration;
        }
        let gap = self
            .last_sequence
            .is_some_and(|last| sequence != last.wrapping_add(1));
        self.last_sequence = Some(sequence);
        if dependency == DependencyClass::Key {
            let recovering = self.state != RecoveryState::Streaming;
            self.state = RecoveryState::Streaming;
            return if recovering {
                FrameDecision::AdmitAndRecover
            } else {
                FrameDecision::Admit
            };
        }
        if self.state != RecoveryState::Streaming {
            return FrameDecision::DropDelta;
        }
        if gap && dependency != DependencyClass::Disposable {
            self.state = RecoveryState::AwaitingRecovery;
            return FrameDecision::DropDelta;
        }
        FrameDecision::Admit
    }

    pub fn unsafe_drop(&mut self) {
        self.state = RecoveryState::AwaitingRecovery;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureAction {
    Feed,
    HoldDisposable,
    RequestRecovery,
}

pub fn decode_pressure_action(
    queue_size: u32,
    oldest_age_ms: u32,
    low: u32,
    high: u32,
    hard_age_ms: u32,
) -> PressureAction {
    if queue_size > high.max(low) || oldest_age_ms > hard_age_ms {
        PressureAction::RequestRecovery
    } else if queue_size >= low {
        PressureAction::HoldDisposable
    } else {
        PressureAction::Feed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ControllerInput {
    pub report_age_ms: u32,
    pub rtt_ms: f64,
    pub baseline_rtt_ms: f64,
    pub oldest_frame_age_ms: f64,
    pub decode_queue: u32,
    pub sequence_gaps: u32,
    pub encoder_pressure: bool,
    pub static_capture: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerAction {
    Hold,
    DecreaseBitrate,
    DecreaseFps,
    DecreaseScale,
    IncreaseBitrate,
}

/// Conservative one-step controller recommendation. State, dwell and hard
/// bounds are owned by the server; this function only classifies one sample.
pub fn recommend_controller_action(input: ControllerInput, age_target_ms: f64) -> ControllerAction {
    if input.report_age_ms > 3_000 || input.static_capture {
        return ControllerAction::Hold;
    }
    if input.encoder_pressure {
        return ControllerAction::DecreaseFps;
    }
    if input.decode_queue > 3 {
        return ControllerAction::DecreaseScale;
    }
    if input.sequence_gaps > 0
        || input.oldest_frame_age_ms > age_target_ms
        || input.rtt_ms > input.baseline_rtt_ms * 1.5
    {
        return ControllerAction::DecreaseBitrate;
    }
    if input.decode_queue == 0
        && input.oldest_frame_age_ms < age_target_ms * 0.5
        && input.rtt_ms < input.baseline_rtt_ms * 1.2
    {
        return ControllerAction::IncreaseBitrate;
    }
    ControllerAction::Hold
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> SizingLimits {
        SizingLimits {
            max_width: 3840,
            max_height: 2160,
            max_pixels: 8_294_400,
            max_dpr: 2.0,
            alignment: 2,
        }
    }

    #[test]
    fn retina_intent_is_honored() {
        let out = compute_effective_sizing(
            SizingIntent {
                css_width: 1512,
                css_height: 800,
                device_pixel_ratio: 2.0,
                render_scale: 1.0,
            },
            limits(),
            None,
        );
        assert_eq!((out.encoded_width, out.encoded_height), (3024, 1600));
        assert_eq!(out.effective_dpr_x, 2.0);
    }

    #[test]
    fn pixel_cap_preserves_aspect() {
        let out = compute_effective_sizing(
            SizingIntent {
                css_width: 3840,
                css_height: 2160,
                device_pixel_ratio: 2.0,
                render_scale: 1.0,
            },
            limits(),
            None,
        );
        assert!(out.encoded_width as u64 * out.encoded_height as u64 <= 8_294_400);
        assert!((out.encoded_width as f64 / out.encoded_height as f64 - 16.0 / 9.0).abs() < 0.01);
        assert!(
            out.limiting_reasons
                .contains(&"server_pixel_cap".to_string())
        );
    }

    #[test]
    fn old_client_can_remain_dpr_one() {
        let out = compute_effective_sizing(
            SizingIntent {
                css_width: 1920,
                css_height: 1080,
                device_pixel_ratio: 1.0,
                render_scale: 1.0,
            },
            limits(),
            None,
        );
        assert_eq!((out.encoded_width, out.encoded_height), (1920, 1080));
    }

    #[test]
    fn clock_estimate_matches_ntp_math() {
        let estimate = estimate_clock_offset(1_000, 1_150, 1_160, 1_110).unwrap();
        assert_eq!(estimate.offset_us, 100);
        assert_eq!(estimate.round_trip_us, 100);
        assert_eq!(estimate.uncertainty_us, 50);
    }

    #[test]
    fn reference_gap_requires_recovery_but_disposable_gap_does_not() {
        let mut m = RecoveryMachine::new(7);
        assert_eq!(
            m.admit(7, 1, DependencyClass::Key),
            FrameDecision::AdmitAndRecover
        );
        assert_eq!(
            m.admit(7, 3, DependencyClass::Disposable),
            FrameDecision::Admit
        );
        assert_eq!(
            m.admit(7, 5, DependencyClass::Reference),
            FrameDecision::DropDelta
        );
        assert_eq!(m.state, RecoveryState::AwaitingRecovery);
        assert_eq!(
            m.admit(7, 6, DependencyClass::Reference),
            FrameDecision::DropDelta
        );
        assert_eq!(
            m.admit(7, 7, DependencyClass::Key),
            FrameDecision::AdmitAndRecover
        );
    }

    #[test]
    fn pressure_is_conservative_for_ippp() {
        assert_eq!(
            decode_pressure_action(4, 20, 1, 3, 100),
            PressureAction::RequestRecovery
        );
        assert_eq!(
            decode_pressure_action(2, 20, 1, 3, 100),
            PressureAction::HoldDisposable
        );
    }

    #[test]
    fn controller_ignores_static_low_fps() {
        let input = ControllerInput {
            report_age_ms: 10,
            rtt_ms: 20.0,
            baseline_rtt_ms: 20.0,
            oldest_frame_age_ms: 1.0,
            decode_queue: 0,
            sequence_gaps: 0,
            encoder_pressure: false,
            static_capture: true,
        };
        assert_eq!(
            recommend_controller_action(input, 100.0),
            ControllerAction::Hold
        );
    }
}
