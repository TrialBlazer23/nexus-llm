//! Local Hexagon prefill decision.
//!
//! The part table is Qualcomm chip ids. Rows with a generation come from
//! LiteRT `supported_soc.csv` (2026-10-11). `SM8735` is v73 from a device
//! log on 2026-10-12, not from that CSV. It is not a phone-name list.
//! Rank and routing read only the resolved token a node advertises.

use crate::config::NpuPrefillMode;
use crate::sysinfo::SystemProfile;
use std::process::Command;
use std::sync::OnceLock;

/// Host facts for `auto`. Tests pass these in. Production reads [`current_host`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NpuHostFacts {
    pub arch_aarch64: bool,
    pub vulkan: bool,
    pub soc_part: Option<String>,
}

impl NpuHostFacts {
    pub fn unqualified() -> Self {
        Self::default()
    }
}

/// Process-lifetime host facts. `getprop` and sysfs run at most once.
pub fn current_host() -> &'static NpuHostFacts {
    static CACHE: OnceLock<NpuHostFacts> = OnceLock::new();
    CACHE.get_or_init(|| NpuHostFacts {
        arch_aarch64: cfg!(target_arch = "aarch64"),
        vulkan: cfg!(target_arch = "aarch64") && SystemProfile::probe_vulkan(),
        soc_part: read_soc_part(),
    })
}

/// Published `(part, HTP generation)` rows that name a generation.
/// Parts with an empty generation in the source CSV are absent.
const HTP_BY_PART: &[(&str, u8)] = &[
    ("QCM6490", 68),
    ("QCM6690", 73),
    ("QCM8550LA", 73),
    ("QCM8550LE", 73),
    ("QCS6690", 73),
    ("QCS7230LA", 66),
    ("QCS7230LE", 66),
    ("QCS8275", 75),
    ("QCS8300", 75),
    ("QCS8625", 75),
    ("QCS9075", 73),
    ("QCS9100", 73),
    ("QRB5165LE", 66),
    ("QRB5165U", 66),
    ("SA8255", 73),
    ("SAR2130P", 73),
    ("SAR2230P", 81),
    ("SC8380XP", 73),
    ("SC8480XP", 81),
    ("SDM845", 65),
    ("SDM865", 66),
    ("SM4350", 66),
    ("SM6125", 66),
    ("SM6150", 66),
    ("SM6225", 66),
    ("SM6350", 66),
    ("SM6375", 66),
    ("SM6450", 73),
    ("SM7125", 66),
    ("SM7150", 65),
    ("SM7225", 66),
    ("SM7250", 66),
    ("SM7315", 68),
    ("SM7325", 68),
    ("SM7350", 68),
    ("SM7435", 73),
    ("SM7450", 69),
    ("SM7475", 69),
    ("SM7550", 73),
    ("SM7675", 73),
    ("SM8150", 66),
    ("SM8250", 66),
    ("SM8350", 68),
    ("SM8350P", 68),
    ("SM8450", 69),
    ("SM8475", 69),
    ("SM8550", 73),
    ("SM8635", 73),
    ("SM8650", 75),
    ("SM8735", 73),
    ("SM8750", 79),
    ("SM8845", 81),
    ("SM8845P", 81),
    ("SM8850", 81),
    ("SSG2115P", 73),
    ("SSG2125P", 73),
    ("SW6100", 81),
    ("SXR1230P", 73),
    ("SXR2230P", 69),
    ("SXR2250P", 69),
    ("SXR2330P", 79),
];

/// Expert-file quant families the Hexagon matmul accepts. Matched as prefixes.
const NPU_QUANT_PREFIXES: &[&str] = &[
    "IQ4_NL", "MXFP4", "Q4_0", "Q4_1", "Q4_K", "Q5_K", "Q6_K", "Q8_0",
];

/// HTP generation for a Qualcomm part, if the published table names one.
pub fn htp_generation(part: &str) -> Option<u8> {
    let key = part.trim().to_ascii_uppercase();
    HTP_BY_PART
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, version)| *version)
}

pub fn htp_generation_qualifies(version: u8) -> bool {
    matches!(version, 73 | 75 | 79 | 81)
}

/// `Some("HTP0")` when this host should pass `--prefill-device`.
///
/// `on` requests the token even for an unknown part. `auto` requests it only
/// on aarch64 Vulkan whose part is a published v73, v75, v79, or v81.
pub fn resolve_prefill(
    mode: NpuPrefillMode,
    arch_aarch64: bool,
    vulkan: bool,
    soc_part: Option<&str>,
) -> Option<&'static str> {
    match mode {
        NpuPrefillMode::Off => None,
        NpuPrefillMode::On => Some("HTP0"),
        NpuPrefillMode::Auto => {
            let version = soc_part.and_then(htp_generation)?;
            if arch_aarch64 && vulkan && htp_generation_qualifies(version) {
                Some("HTP0")
            } else {
                None
            }
        }
    }
}

/// Unknown labels do not block. A known label outside the NPU families does.
pub fn quant_allows_npu_prefill(label: Option<&str>) -> bool {
    let Some(label) = label.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let upper = label.to_ascii_uppercase();
    NPU_QUANT_PREFIXES
        .iter()
        .any(|prefix| upper.starts_with(prefix))
}

/// Resolved token after the quant gate. `on` does not override a known-bad quant.
pub fn prefill_token(
    mode: NpuPrefillMode,
    host: &NpuHostFacts,
    quant_label: Option<&str>,
) -> Option<&'static str> {
    let token = resolve_prefill(
        mode,
        host.arch_aarch64,
        host.vulkan,
        host.soc_part.as_deref(),
    )?;
    if quant_allows_npu_prefill(quant_label) {
        Some(token)
    } else {
        None
    }
}

pub fn npu_prefill_line(mode: NpuPrefillMode, host: &NpuHostFacts) -> String {
    match mode {
        NpuPrefillMode::Off => "npu prefill: off".to_string(),
        NpuPrefillMode::On => "npu prefill: on (HTP0)".to_string(),
        NpuPrefillMode::Auto => {
            if resolve_prefill(
                mode,
                host.arch_aarch64,
                host.vulkan,
                host.soc_part.as_deref(),
            )
            .is_some()
            {
                "npu prefill: auto (HTP0)".to_string()
            } else {
                "npu prefill: auto (off)".to_string()
            }
        }
    }
}

/// First Qualcomm-style part token in `text`.
pub fn find_soc_part(text: &str) -> Option<String> {
    let mut token = String::new();
    let mut best: Option<String> = None;
    for ch in text.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() {
            token.push(ch.to_ascii_uppercase());
            continue;
        }
        if looks_like_soc_part(&token) {
            best = Some(token.clone());
            break;
        }
        token.clear();
    }
    best
}

fn looks_like_soc_part(token: &str) -> bool {
    if token.len() < 4 || token.len() > 12 {
        return false;
    }
    if !token.bytes().any(|byte| byte.is_ascii_digit()) {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "QCM", "QCS", "QRB", "SAR", "SDM", "SSG", "SXR", "SM", "SC", "SW", "SA",
    ];
    PREFIXES.iter().any(|prefix| token.starts_with(prefix))
}

/// Read `/sys/devices/soc0/machine`, then `getprop ro.soc.model`. Cached.
pub fn read_soc_part() -> Option<String> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE.get_or_init(read_soc_part_uncached).clone()
}

fn read_soc_part_uncached() -> Option<String> {
    if let Ok(text) = std::fs::read_to_string("/sys/devices/soc0/machine") {
        if let Some(part) = find_soc_part(&text) {
            return Some(part);
        }
    }
    let output = Command::new("getprop").arg("ro.soc.model").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    find_soc_part(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(arch: bool, vulkan: bool, part: Option<&str>) -> NpuHostFacts {
        NpuHostFacts {
            arch_aarch64: arch,
            vulkan,
            soc_part: part.map(str::to_string),
        }
    }

    #[test]
    fn published_v73_qualifies_and_unknown_part_does_not() {
        assert_eq!(htp_generation("SM8550"), Some(73));
        assert_eq!(htp_generation("sm8650"), Some(75));
        assert_eq!(htp_generation("SM8750"), Some(79));
        assert_eq!(htp_generation("SM8850"), Some(81));
        assert_eq!(htp_generation("SM8450"), Some(69));
        assert_eq!(htp_generation("SM8735"), Some(73));
        assert_eq!(htp_generation("SM0000"), None);
        assert!(htp_generation_qualifies(73));
        assert!(!htp_generation_qualifies(69));
    }

    #[test]
    fn resolve_prefill_matrix() {
        let sm8550 = Some("SM8550");
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, true, sm8550),
            Some("HTP0")
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, true, Some("SM8735")),
            Some("HTP0")
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, true, Some("SM0000")),
            None
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, false, sm8550),
            None
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, false, true, sm8550),
            None
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, true, Some("SM8450")),
            None
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::On, true, true, Some("SM0000")),
            Some("HTP0")
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Off, true, true, sm8550),
            None
        );
        assert_eq!(
            resolve_prefill(NpuPrefillMode::Auto, true, true, None),
            None
        );
    }

    #[test]
    fn quant_gate_allows_q4_k_and_blocks_q3() {
        assert!(quant_allows_npu_prefill(None));
        assert!(quant_allows_npu_prefill(Some("")));
        assert!(quant_allows_npu_prefill(Some("Q4_K_M")));
        assert!(quant_allows_npu_prefill(Some("q8_0")));
        assert!(quant_allows_npu_prefill(Some("IQ4_NL")));
        assert!(!quant_allows_npu_prefill(Some("Q3_K_M")));
        assert!(!quant_allows_npu_prefill(Some("Q2_K")));
        let facts = host(true, true, Some("SM8550"));
        assert_eq!(
            prefill_token(NpuPrefillMode::On, &facts, Some("Q3_K_M")),
            None
        );
        assert_eq!(
            prefill_token(NpuPrefillMode::On, &facts, Some("Q4_K_M")),
            Some("HTP0")
        );
    }

    #[test]
    fn doctor_lines_follow_mode_and_resolution() {
        let qualified = host(true, true, Some("SM8550"));
        let measured = host(true, true, Some("SM8735"));
        let unknown = host(true, true, Some("SM0000"));
        assert_eq!(
            npu_prefill_line(NpuPrefillMode::Off, &qualified),
            "npu prefill: off"
        );
        assert_eq!(
            npu_prefill_line(NpuPrefillMode::On, &unknown),
            "npu prefill: on (HTP0)"
        );
        assert_eq!(
            npu_prefill_line(NpuPrefillMode::Auto, &qualified),
            "npu prefill: auto (HTP0)"
        );
        assert_eq!(
            npu_prefill_line(NpuPrefillMode::Auto, &measured),
            "npu prefill: auto (HTP0)"
        );
        assert_eq!(
            npu_prefill_line(NpuPrefillMode::Auto, &unknown),
            "npu prefill: auto (off)"
        );
    }

    #[test]
    fn find_soc_part_reads_a_machine_string() {
        assert_eq!(
            find_soc_part("Qualcomm Technologies, Inc SM8550\n").as_deref(),
            Some("SM8550")
        );
        assert_eq!(find_soc_part("SM8735").as_deref(), Some("SM8735"));
        assert_eq!(find_soc_part("not a chip"), None);
    }
}
