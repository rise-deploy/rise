//! Which ECS capacity carries the workload services.
//!
//! ECS expresses this two mutually exclusive ways on `CreateService`: a
//! `launchType` (`FARGATE` or `EC2`) or a `capacityProviderStrategy` naming
//! providers the cluster already has attached. Passing both is an error, which
//! is why this is one enum rather than two independent settings.
//!
//! Networking is deliberately *not* a variant here. Every capacity Rise supports
//! runs on `awsvpc`, because the readiness path keys Traefik's `serverStatus` on
//! each task's own ENI address; `bridge` would need the container instance's IP
//! and host port, which the controller has no permission to read.

use anyhow::{bail, Result};

/// Which `requiresCompatibilities` a task definition needs.
///
/// Distinct from [`Capacity`] because a capacity provider's name does not imply
/// it: `FARGATE_SPOT` is Fargate-compatible, an Auto Scaling group provider is
/// EC2-compatible, and ECS infers neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    Fargate,
    Ec2,
}

impl Compatibility {
    /// The token ECS wants in `requiresCompatibilities`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Compatibility::Fargate => "FARGATE",
            Compatibility::Ec2 => "EC2",
        }
    }

    /// Whether task CPU/memory are restricted to Fargate's discrete size table.
    ///
    /// EC2 capacity takes arbitrary task-level values, bounded by what the
    /// container instance has rather than by a table.
    pub fn uses_fargate_size_table(&self) -> bool {
        matches!(self, Compatibility::Fargate)
    }

    /// Whether `assignPublicIp` may appear in the `awsvpc` configuration at all.
    ///
    /// ECS rejects the field on EC2 capacity -- "Assign public IP is not
    /// supported for this launch type" -- rather than ignoring it, so sending
    /// even `DISABLED` there is a needless risk. An `awsvpc` task ENI on a
    /// container instance never receives a public address; egress is the
    /// subnet's job.
    pub fn supports_assign_public_ip(&self) -> bool {
        matches!(self, Compatibility::Fargate)
    }
}

/// A named capacity provider the cluster already has attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityProvider {
    pub name: String,
    /// What the task definition must declare. Not derivable from `name`.
    pub compatibility: Compatibility,
    pub base: Option<i32>,
    pub weight: Option<i32>,
}

/// Where a workload service's tasks run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Capacity {
    /// `launchType: FARGATE`. AWS schedules the task; nothing to operate.
    #[default]
    Fargate,
    /// `launchType: EC2`. Tasks are placed on the cluster's container
    /// instances, which the operator runs. Still `awsvpc`, so each task takes
    /// an ENI — enable ENI trunking or the per-instance attachment limit
    /// becomes the real ceiling on tasks per host.
    Ec2,
    /// `capacityProviderStrategy` naming one provider.
    ///
    /// Not reachable from configuration yet. The variant exists so that
    /// interruptible capacity (`FARGATE_SPOT`, or a Spot-backed Auto Scaling
    /// group provider) becomes a settings value and a `rise.toml` knob rather
    /// than a new branch at every call site that places a task.
    Provider(CapacityProvider),
}

/// The canonical token for a configured capacity, or an error naming what works.
///
/// Rejecting at load is the point: a capacity we do not recognise has no safe
/// default. Falling back to Fargate would silently ignore an operator who asked
/// for EC2 and bill them for serverless capacity they did not want, and passing
/// the string through would surface as an AWS error on every deploy instead of
/// once at startup.
pub fn canonical(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_uppercase().as_str() {
        "FARGATE" => Ok("FARGATE"),
        "EC2" => Ok("EC2"),
        // Quote what was configured, not the normalised form: an operator
        // hunting for the offending value greps for what they typed.
        _ => bail!(
            "capacity {raw:?} is not an ECS capacity Rise supports. Use \
             \"fargate\" or \"ec2\"."
        ),
    }
}

impl Capacity {
    /// Parse a configured value. Accepts any casing; see [`canonical`].
    pub fn parse(raw: &str) -> Result<Self> {
        match canonical(raw)? {
            "EC2" => Ok(Capacity::Ec2),
            _ => Ok(Capacity::Fargate),
        }
    }

    pub fn compatibility(&self) -> Compatibility {
        match self {
            Capacity::Fargate => Compatibility::Fargate,
            Capacity::Ec2 => Compatibility::Ec2,
            Capacity::Provider(p) => p.compatibility,
        }
    }

    /// The `launchType` to send, or `None` when this capacity is expressed as a
    /// strategy instead. Exactly one of this and [`Self::provider`] is `Some`.
    pub fn launch_type(&self) -> Option<&'static str> {
        match self {
            Capacity::Fargate => Some("FARGATE"),
            Capacity::Ec2 => Some("EC2"),
            Capacity::Provider(_) => None,
        }
    }

    /// The provider to send as a one-item `capacityProviderStrategy`.
    pub fn provider(&self) -> Option<&CapacityProvider> {
        match self {
            Capacity::Provider(p) => Some(p),
            _ => None,
        }
    }

    /// How to name this capacity in an operator-facing message.
    pub fn describe(&self) -> String {
        match self {
            Capacity::Fargate => "launch type FARGATE".to_string(),
            Capacity::Ec2 => "launch type EC2".to_string(),
            Capacity::Provider(p) => format!("capacity provider {}", p.name),
        }
    }

    /// Whether a service ECS reports already runs on this capacity.
    ///
    /// `DescribeServices` reports whichever form the service was created with:
    /// a `launchType` **or** a strategy, never both. A service created with a
    /// launch type therefore never matches a desired provider, and vice versa —
    /// which is correct, because those are exactly the transitions ECS will not
    /// perform in place.
    pub fn matches_observed(&self, launch_type: Option<&str>, providers: &[String]) -> bool {
        match self {
            Capacity::Fargate | Capacity::Ec2 => {
                providers.is_empty()
                    && launch_type.is_some_and(|lt| {
                        lt.eq_ignore_ascii_case(self.launch_type().expect("not a provider"))
                    })
            }
            Capacity::Provider(p) => {
                providers.len() == 1 && providers[0].eq_ignore_ascii_case(&p.name)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_accepts_either_capacity_in_any_casing() {
        for (raw, want) in [
            ("fargate", "FARGATE"),
            ("FARGATE", "FARGATE"),
            ("  Fargate  ", "FARGATE"),
            ("ec2", "EC2"),
            ("Ec2", "EC2"),
        ] {
            assert_eq!(canonical(raw).unwrap(), want, "for {raw:?}");
        }
    }

    #[test]
    fn an_unknown_capacity_is_refused_and_the_message_names_what_works() {
        // Silently defaulting would bill an operator who asked for EC2 for
        // serverless capacity they never wanted.
        let err = canonical("spot").unwrap_err().to_string();
        assert!(err.contains("spot"), "{err}");
        assert!(err.contains("fargate"), "{err}");
        assert!(err.contains("ec2"), "{err}");

        assert!(canonical("").is_err());
        assert!(canonical("external").is_err());
    }

    #[test]
    fn the_default_capacity_is_fargate() {
        // An install that never sets the key must keep behaving exactly as it
        // did before the key existed.
        assert_eq!(Capacity::default(), Capacity::Fargate);
        assert_eq!(Capacity::parse("fargate").unwrap(), Capacity::Fargate);
        assert_eq!(Capacity::parse("ec2").unwrap(), Capacity::Ec2);
    }

    #[test]
    fn launch_type_and_provider_are_mutually_exclusive() {
        // ECS rejects a CreateService carrying both, so exactly one must be Some
        // for every variant.
        for capacity in [
            Capacity::Fargate,
            Capacity::Ec2,
            Capacity::Provider(CapacityProvider {
                name: "FARGATE_SPOT".to_string(),
                compatibility: Compatibility::Fargate,
                base: None,
                weight: Some(1),
            }),
        ] {
            assert_eq!(
                capacity.launch_type().is_some(),
                capacity.provider().is_none(),
                "for {capacity:?}"
            );
        }
    }

    #[test]
    fn compatibility_follows_the_capacity_not_the_provider_name() {
        assert_eq!(Capacity::Fargate.compatibility(), Compatibility::Fargate);
        assert_eq!(Capacity::Ec2.compatibility(), Compatibility::Ec2);

        // A Spot-backed Auto Scaling group provider is EC2-compatible even
        // though a Fargate one would not be; the name says nothing.
        let asg = Capacity::Provider(CapacityProvider {
            name: "spot-asg".to_string(),
            compatibility: Compatibility::Ec2,
            base: None,
            weight: Some(1),
        });
        assert_eq!(asg.compatibility(), Compatibility::Ec2);
        assert_eq!(asg.compatibility().as_str(), "EC2");
    }

    #[test]
    fn only_fargate_compatibility_uses_the_size_table() {
        assert!(Compatibility::Fargate.uses_fargate_size_table());
        assert!(!Compatibility::Ec2.uses_fargate_size_table());
    }

    #[test]
    fn matches_observed_reads_a_launch_type_service() {
        assert!(Capacity::Fargate.matches_observed(Some("FARGATE"), &[]));
        assert!(Capacity::Ec2.matches_observed(Some("EC2"), &[]));

        assert!(!Capacity::Fargate.matches_observed(Some("EC2"), &[]));
        assert!(!Capacity::Ec2.matches_observed(Some("FARGATE"), &[]));
        assert!(!Capacity::Fargate.matches_observed(None, &[]));
    }

    #[test]
    fn matches_observed_reads_a_capacity_provider_service() {
        let spot = Capacity::Provider(CapacityProvider {
            name: "FARGATE_SPOT".to_string(),
            compatibility: Compatibility::Fargate,
            base: None,
            weight: Some(1),
        });
        assert!(spot.matches_observed(None, &["FARGATE_SPOT".to_string()]));
        assert!(spot.matches_observed(None, &["fargate_spot".to_string()]));

        assert!(!spot.matches_observed(None, &["FARGATE".to_string()]));
        assert!(!spot.matches_observed(None, &[]));
        // A mixed strategy is not what we asked for, even if ours is in it.
        assert!(!spot.matches_observed(None, &["FARGATE".to_string(), "FARGATE_SPOT".to_string()]));
    }

    #[test]
    fn a_launch_type_service_never_matches_a_provider_and_vice_versa() {
        // These are exactly the transitions ECS will not perform in place, so
        // the mismatch must be visible rather than papered over.
        let spot = Capacity::Provider(CapacityProvider {
            name: "FARGATE_SPOT".to_string(),
            compatibility: Compatibility::Fargate,
            base: None,
            weight: Some(1),
        });
        assert!(!spot.matches_observed(Some("FARGATE"), &[]));
        assert!(!Capacity::Fargate.matches_observed(None, &["FARGATE".to_string()]));
    }
}
