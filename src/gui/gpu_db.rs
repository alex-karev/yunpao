//! What a beginner needs to know about a GPU model: how new it is (bf16 / flash-attention
//! need Ampere or later) and, with its memory, what it is good for. Static table, no AI.

use serde::Serialize;

#[derive(Serialize, Clone, Default, Debug)]
pub struct GpuProfile {
    /// Architecture, e.g. "Ada Lovelace".
    pub arch: &'static str,
    /// CUDA compute capability ×10 (80 = sm_80, Ampere).
    pub sm: u32,
    /// bf16 and flash-attention work (sm_80+).
    pub bf16: bool,
    /// One line for beginners: memory, speed tier, what it suits.
    pub blurb: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Tier {
    /// H100-class: fastest, scarce.
    Top,
    Datacenter,
    Desktop,
    Old,
}

/// Longer / more specific names first: "A100" before "A10", "L40S" before "L4".
const MODELS: &[(&str, &str, u32, Tier)] = &[
    ("B200", "Blackwell", 100, Tier::Top),
    ("B100", "Blackwell", 100, Tier::Top),
    ("GH200", "Hopper", 90, Tier::Top),
    ("H200", "Hopper", 90, Tier::Top),
    ("H100", "Hopper", 90, Tier::Top),
    ("H800", "Hopper", 90, Tier::Top),
    ("H20", "Hopper", 90, Tier::Datacenter),
    ("RTX PRO 6000", "Blackwell", 120, Tier::Datacenter),
    ("5090", "Blackwell", 120, Tier::Desktop),
    ("5080", "Blackwell", 120, Tier::Desktop),
    ("L40S", "Ada Lovelace", 89, Tier::Datacenter),
    ("L40", "Ada Lovelace", 89, Tier::Datacenter),
    ("L20", "Ada Lovelace", 89, Tier::Datacenter),
    ("L4", "Ada Lovelace", 89, Tier::Datacenter),
    ("RTX 6000 Ada", "Ada Lovelace", 89, Tier::Datacenter),
    ("RTX 5000 Ada", "Ada Lovelace", 89, Tier::Desktop),
    ("RTX 4500 Ada", "Ada Lovelace", 89, Tier::Desktop),
    ("RTX 4000 Ada", "Ada Lovelace", 89, Tier::Desktop),
    ("4090", "Ada Lovelace", 89, Tier::Desktop),
    ("4080", "Ada Lovelace", 89, Tier::Desktop),
    ("4070", "Ada Lovelace", 89, Tier::Desktop),
    ("4060", "Ada Lovelace", 89, Tier::Desktop),
    ("A100", "Ampere", 80, Tier::Datacenter),
    ("A800", "Ampere", 80, Tier::Datacenter),
    ("A30", "Ampere", 80, Tier::Datacenter),
    ("A6000", "Ampere", 86, Tier::Datacenter),
    ("A5000", "Ampere", 86, Tier::Desktop),
    ("A4000", "Ampere", 86, Tier::Desktop),
    ("A40", "Ampere", 86, Tier::Datacenter),
    ("A10", "Ampere", 86, Tier::Datacenter),
    ("3090", "Ampere", 86, Tier::Desktop),
    ("3080", "Ampere", 86, Tier::Desktop),
    ("3070", "Ampere", 86, Tier::Desktop),
    ("3060", "Ampere", 86, Tier::Desktop),
    ("V100", "Volta", 70, Tier::Old),
    ("TITAN V", "Volta", 70, Tier::Old),
    ("TITAN RTX", "Turing", 75, Tier::Old),
    ("Quadro RTX", "Turing", 75, Tier::Old),
    ("2080", "Turing", 75, Tier::Old),
    ("2070", "Turing", 75, Tier::Old),
    ("2060", "Turing", 75, Tier::Old),
    ("T4", "Turing", 75, Tier::Old),
    ("P100", "Pascal", 60, Tier::Old),
    ("P40", "Pascal", 61, Tier::Old),
    ("1080", "Pascal", 61, Tier::Old),
    ("1070", "Pascal", 61, Tier::Old),
    ("TITAN X", "Pascal", 61, Tier::Old),
    ("K80", "Kepler", 37, Tier::Old),
];

/// What fits in this much memory (GiB), for a beginner.
fn suits(mem_gib: u32) -> &'static str {
    match mem_gib {
        75.. => "大模型训练/微调都行（13B~70B 用 LoRA）",
        40.. => "大多数训练，7B~13B 推理、7B LoRA 微调",
        20.. => "常规 CV/NLP 训练，7B 推理要量化",
        10.. => "小模型训练、课程作业",
        _ => "入门实验、小模型",
    }
}

pub fn profile(name: &str, mem_total_mib: u32) -> Option<GpuProfile> {
    let (_, arch, sm, tier) = MODELS.iter().find(|(pat, ..)| name.contains(pat))?;
    let gib = (mem_total_mib + 512) / 1024;
    let speed = match tier {
        Tier::Top => "算力最强，稀缺，小任务别占",
        Tier::Datacenter => "数据中心卡，稳定",
        Tier::Desktop => "桌面卡，性价比高",
        Tier::Old => "老架构，不支持 bf16 / flash-attention",
    };
    Some(GpuProfile {
        arch,
        sm: *sm,
        bf16: *sm >= 80,
        blurb: format!("{arch} · {gib}G · {speed} · 适合{}", suits(gib)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_specific_names_first() {
        assert_eq!(profile("NVIDIA RTX 6000 Ada Generation", 49140).unwrap().sm, 89);
        assert_eq!(profile("NVIDIA H100 PCIe", 81559).unwrap().arch, "Hopper");
        assert_eq!(profile("NVIDIA A100-SXM4-80GB", 81920).unwrap().sm, 80);
        assert_eq!(profile("NVIDIA A10", 23028).unwrap().sm, 86);
        assert_eq!(profile("NVIDIA RTX A4000", 16376).unwrap().arch, "Ampere");
        assert_eq!(profile("NVIDIA L40S", 46068).unwrap().arch, "Ada Lovelace");
        assert!(!profile("Tesla V100-SXM2-32GB", 32768).unwrap().bf16);
        assert!(profile("NVIDIA GeForce RTX 3090", 24576).unwrap().blurb.contains("24G"));
        assert!(profile("Some Unknown GPU", 8000).is_none());
    }
}
