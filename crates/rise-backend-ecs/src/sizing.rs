//! Fargate task sizing: mapping Rise's free-form `cpu`/`memory` requests onto
//! the discrete set of CPU/memory combinations Fargate actually accepts.
//!
//! Rise lets users ask for any quantity (`"500m"`, `"1.5"`, `"768Mi"`, or a
//! `request-limit` range). Fargate accepts only a fixed table of pairs, and
//! rejects anything else at `RegisterTaskDefinition`. Rather than surface that
//! as an opaque AWS error at reconcile time, we resolve the request here — at
//! deploy time, against a pure table — and either round **up** to the smallest
//! combination that satisfies both dimensions or reject with an error naming
//! what would work.
//!
//! Rounding up (never down) is the invariant: a workload must never receive less
//! CPU or memory than it asked for. The consequence is billing-visible and worth
//! stating plainly — Rise's own defaults (`500m` / `256Mi`) resolve to 512 CPU
//! units, and 512 CPU units require at least 1024 MiB, so a default deployment
//! runs as 0.5 vCPU / 1 GB. Callers surface [`FargateSize::rounded_up`] so that
//! is visible rather than a silent 4× on the memory line of an invoice.

use anyhow::Result;
use rise_backend_core::quantity::{
    parse_cpu_millicores, parse_cpu_request_limit, parse_memory_bytes, parse_memory_request_limit,
};

/// One row of the Fargate sizing table: a CPU value and the memory range it
/// admits, in fixed increments.
struct CpuRow {
    /// CPU units (1024 = 1 vCPU).
    cpu: u32,
    /// Smallest admissible memory, MiB.
    min_mib: u32,
    /// Largest admissible memory, MiB.
    max_mib: u32,
    /// Increment between admissible memory values, MiB.
    step_mib: u32,
}

/// The Fargate CPU/memory table (platform version 1.4.0+).
///
/// Ordered by CPU ascending so the first row that fits is also the cheapest.
const TABLE: &[CpuRow] = &[
    // 0.25 vCPU takes only three discrete values, not a range.
    CpuRow {
        cpu: 256,
        min_mib: 512,
        max_mib: 2048,
        step_mib: 512,
    },
    CpuRow {
        cpu: 512,
        min_mib: 1024,
        max_mib: 4096,
        step_mib: 1024,
    },
    CpuRow {
        cpu: 1024,
        min_mib: 2048,
        max_mib: 8192,
        step_mib: 1024,
    },
    CpuRow {
        cpu: 2048,
        min_mib: 4096,
        max_mib: 16384,
        step_mib: 1024,
    },
    CpuRow {
        cpu: 4096,
        min_mib: 8192,
        max_mib: 30720,
        step_mib: 1024,
    },
    CpuRow {
        cpu: 8192,
        min_mib: 16384,
        max_mib: 61440,
        step_mib: 4096,
    },
    CpuRow {
        cpu: 16384,
        min_mib: 32768,
        max_mib: 122880,
        step_mib: 8192,
    },
];

/// A resolved Fargate task size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FargateSize {
    /// CPU units for the task definition's `cpu` field (1024 = 1 vCPU).
    pub cpu_units: u32,
    /// Memory MiB for the task definition's `memory` field.
    pub memory_mib: u32,
    /// Whether either dimension had to be raised above what was requested.
    /// Callers log this so the billing consequence is visible.
    pub rounded_up: bool,
}

impl FargateSize {
    /// The task definition's `cpu` value, as ECS wants it (a decimal string).
    pub fn cpu_string(&self) -> String {
        self.cpu_units.to_string()
    }

    /// The task definition's `memory` value, as ECS wants it (MiB, decimal string).
    pub fn memory_string(&self) -> String {
        self.memory_mib.to_string()
    }
}

/// The `256` row admits 512/1024/2048 — a step of 512 from 512 would also yield
/// 1536, which Fargate rejects. Encode the exception explicitly.
const CPU_256_VALUES: &[u32] = &[512, 1024, 2048];

/// Smallest admissible memory in a row that is >= `want_mib`, or `None` if the
/// row's ceiling is too low.
fn fit_memory(row: &CpuRow, want_mib: u32) -> Option<u32> {
    if want_mib > row.max_mib {
        return None;
    }
    if row.cpu == 256 {
        return CPU_256_VALUES.iter().copied().find(|v| *v >= want_mib);
    }
    if want_mib <= row.min_mib {
        return Some(row.min_mib);
    }
    // Round up to the next step boundary above min.
    let over = want_mib - row.min_mib;
    let steps = over.div_ceil(row.step_mib);
    Some(row.min_mib + steps * row.step_mib)
}

/// Resolve a Rise `cpu`/`memory` request onto the Fargate table.
///
/// Both inputs accept a bare quantity (`"500m"`, `"1"`, `"256Mi"`) or a
/// `request-limit` range (`"500m-1"`); the **limit** half is used, matching what
/// the Docker backend applies as its hard cap so the same public input means the
/// same ceiling on both backends.
///
/// Returns the smallest combination satisfying both dimensions. Errors when the
/// request exceeds the largest Fargate size, naming the maximum — an actionable
/// message beats `RegisterTaskDefinition` failing with "Invalid CPU or memory".
pub fn resolve(cpu: &str, memory: &str) -> Result<FargateSize> {
    let (_, cpu_limit) = parse_cpu_request_limit(cpu)
        .map_err(|e| anyhow::anyhow!("invalid cpu value {cpu:?}: {e}"))?;
    let (_, memory_limit) = parse_memory_request_limit(memory)
        .map_err(|e| anyhow::anyhow!("invalid memory value {memory:?}: {e}"))?;

    let want_millicores = parse_cpu_millicores(&cpu_limit)
        .map_err(|e| anyhow::anyhow!("invalid cpu value {cpu:?}: {e}"))?;
    let want_bytes = parse_memory_bytes(&memory_limit)
        .map_err(|e| anyhow::anyhow!("invalid memory value {memory:?}: {e}"))?;

    // 1 vCPU = 1000 millicores = 1024 CPU units. A request so large its unit or
    // MiB count overflows u32 cannot fit any Fargate size, and a truncating cast
    // would silently wrap it down to a small valid one -- under-provisioning a
    // workload that should be rejected outright. Treat overflow as "too large".
    let too_large = || {
        let max = TABLE.last().expect("table is non-empty");
        anyhow::anyhow!(
            "cpu {cpu:?} / memory {memory:?} exceeds the largest Fargate task size \
             ({} vCPU / {} GiB). Reduce the request, or run this workload on a \
             backend without Fargate's fixed size table.",
            max.cpu / 1024,
            max.max_mib / 1024,
        )
    };
    let want_cpu_units = u32::try_from(want_millicores.saturating_mul(1024).div_ceil(1000))
        .map_err(|_| too_large())?;
    let want_mib = u32::try_from(want_bytes.div_ceil(1024 * 1024)).map_err(|_| too_large())?;

    for row in TABLE {
        if row.cpu < want_cpu_units {
            continue;
        }
        if let Some(memory_mib) = fit_memory(row, want_mib) {
            return Ok(FargateSize {
                cpu_units: row.cpu,
                memory_mib,
                rounded_up: row.cpu != want_cpu_units || memory_mib != want_mib,
            });
        }
    }

    Err(too_large())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination we can emit must be one Fargate actually accepts —
    /// otherwise the failure surfaces as an opaque `RegisterTaskDefinition`
    /// error at reconcile time, long after the user's deploy returned.
    fn assert_valid_combination(size: FargateSize) {
        let row = TABLE
            .iter()
            .find(|r| r.cpu == size.cpu_units)
            .unwrap_or_else(|| panic!("cpu {} is not in the table", size.cpu_units));
        assert!(
            size.memory_mib >= row.min_mib && size.memory_mib <= row.max_mib,
            "memory {} outside row {}'s range {}..={}",
            size.memory_mib,
            row.cpu,
            row.min_mib,
            row.max_mib
        );
        if row.cpu == 256 {
            assert!(
                CPU_256_VALUES.contains(&size.memory_mib),
                "memory {} is not one of the three values the 256 row admits",
                size.memory_mib
            );
        } else {
            assert_eq!(
                (size.memory_mib - row.min_mib) % row.step_mib,
                0,
                "memory {} is not on row {}'s {} MiB step boundary",
                size.memory_mib,
                row.cpu,
                row.step_mib
            );
        }
    }

    #[test]
    fn rise_defaults_round_up_to_half_vcpu_and_one_gib() {
        // The headline case: Rise's own defaults. 500m -> 512 CPU units, and the
        // 512 row's floor is 1024 MiB, so the requested 256Mi is raised 4x. That
        // is billing-visible, so `rounded_up` must be set for the caller to log.
        let size = resolve("500m", "256Mi").expect("defaults must resolve");
        assert_eq!(size.cpu_units, 512);
        assert_eq!(size.memory_mib, 1024);
        assert!(size.rounded_up);
        assert_valid_combination(size);
    }

    #[test]
    fn exact_table_hit_is_not_flagged_as_rounded() {
        // 1 vCPU / 2Gi is exactly a table row: nothing was raised, so a caller
        // logging `rounded_up` must not cry wolf.
        let size = resolve("1", "2Gi").expect("exact combination resolves");
        assert_eq!(size.cpu_units, 1024);
        assert_eq!(size.memory_mib, 2048);
        assert!(!size.rounded_up);
    }

    #[test]
    fn memory_beyond_a_row_ceiling_moves_to_the_next_cpu_row() {
        // 256 CPU units tops out at 2048 MiB. Asking for 0.25 vCPU / 3Gi cannot
        // be served by that row, and silently truncating memory would starve the
        // workload — so it must climb to the next CPU row instead.
        let size = resolve("250m", "3Gi").expect("resolves by climbing a row");
        assert_eq!(size.cpu_units, 512);
        assert_eq!(size.memory_mib, 3072);
        assert!(size.rounded_up);
        assert_valid_combination(size);
    }

    #[test]
    fn memory_rounds_up_to_the_step_boundary_never_down() {
        // 1500 MiB on the 512 row must become 2048, not 1024: rounding down
        // would hand the container less memory than it asked for and it would
        // OOM under exactly the load it was sized for.
        let size = resolve("500m", "1500Mi").expect("resolves");
        assert_eq!(size.cpu_units, 512);
        assert_eq!(size.memory_mib, 2048);
        assert_valid_combination(size);
    }

    #[test]
    fn the_256_row_skips_the_invalid_1536_value() {
        // A naive `min + n*step` on the 256 row would emit 1536 MiB, which
        // Fargate rejects. Only 512/1024/2048 are legal there.
        let size = resolve("200m", "1100Mi").expect("resolves");
        assert_eq!(size.cpu_units, 256);
        assert_eq!(size.memory_mib, 2048);
        assert_valid_combination(size);
    }

    #[test]
    fn range_form_uses_the_limit_half() {
        // `request-limit` ranges are a public Rise input. The Docker backend
        // applies the limit half as its hard cap; ECS must size to the same
        // number or the two backends give different ceilings for one input.
        let size = resolve("250m-1", "512Mi-2Gi").expect("range resolves");
        assert_eq!(size.cpu_units, 1024);
        assert_eq!(size.memory_mib, 2048);
        assert_valid_combination(size);
    }

    #[test]
    fn a_request_past_the_u32_boundary_is_rejected_not_wrapped() {
        // The unit count for this request overflows u32; a truncating cast would
        // wrap it down to a small in-table size and silently under-provision.
        // It must be rejected with the same "too large" error instead.
        let err = resolve("4194305", "256Mi").expect_err("beyond u32 of CPU units");
        assert!(
            err.to_string()
                .contains("exceeds the largest Fargate task size"),
            "must reject, not wrap: {err}"
        );
    }

    #[test]
    fn oversized_request_is_rejected_with_an_actionable_message() {
        // Better a clear deploy-time error than an opaque AWS "Invalid CPU or
        // memory" at reconcile time, after the CLI has already returned success.
        let err = resolve("32", "256Gi").expect_err("beyond the largest task size");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds the largest Fargate task size"),
            "unhelpful message: {msg}"
        );
        assert!(msg.contains("16 vCPU"), "message must name the max: {msg}");
    }

    #[test]
    fn every_row_boundary_resolves_to_itself() {
        // Sweep the table: each row's own floor and ceiling must resolve back to
        // that exact pair. Guards against an off-by-one in `fit_memory` that
        // would push a valid request up a row (and double the bill).
        for row in TABLE {
            let cpu = format!("{}m", (row.cpu as u64 * 1000).div_ceil(1024));
            for mib in [row.min_mib, row.max_mib] {
                let size = resolve(&cpu, &format!("{mib}Mi"))
                    .unwrap_or_else(|e| panic!("row {} / {mib}Mi must resolve: {e}", row.cpu));
                assert_eq!(size.cpu_units, row.cpu, "row {} floor/ceiling", row.cpu);
                assert_eq!(size.memory_mib, mib, "row {} floor/ceiling", row.cpu);
                assert_valid_combination(size);
            }
        }
    }
}
