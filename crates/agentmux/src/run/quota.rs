//! The usage windows a consultation reports.

use std::path::PathBuf;

use crate::config::{Config, ConfigError, ModelRules};
use crate::delegate::Vendor;
use crate::transcript::{FailureKind, Outcome};

use super::RunStore;
use super::error::RunError;
use super::id::RunId;
use super::records::{Meta, write_json};

impl RunStore {
    /// The other accounts' figures, when this consultation was refused for a rate limit.
    ///
    /// Probed only on that failure, where the answer is exactly what the caller needs and the
    /// figures cannot be stale in the way that matters: the window just closed.
    /// Written beside the run so repeated `status` calls cost nothing, and so the record of why a
    /// consultation was told to go elsewhere survives with the consultation.
    pub(super) fn quota_for_failure(
        &self,
        run_id: &RunId,
        meta: &Meta,
        outcome: &Outcome,
    ) -> Vec<crate::quota::AccountQuota> {
        if !matches!(
            outcome,
            Outcome::Failed {
                kind: FailureKind::RateLimited,
                ..
            }
        ) {
            return Vec::new();
        }
        let path = self.run_dir(run_id).join("quota.json");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(cached) = serde_json::from_str(&text)
        {
            return cached;
        }
        let Ok(config) = crate::config::Config::load(&self.host_env, &meta.cwd) else {
            return Vec::new();
        };
        let reported = crate::quota::probe_vendor(
            meta.delegate.vendor(),
            &config,
            &self.host_env,
            self.probe.as_ref(),
        );
        // Best effort: a consultation that cannot cache its quota still reports it.
        write_json(&path, &reported)
            .inspect_err(|err| tracing::warn!(?err, "cannot cache a consultation's quota"))
            .ok();
        reported
    }

    /// Ask each configured account what it has left, for `vendor` or for both.
    ///
    /// The configuration is read afresh for every call, as a launch reads it.
    /// A project file found from agentmux's own directory still decides which identity an
    /// unnamed account resolves to, as it would for a consultation started there; when it cannot
    /// be read, the accounts are reported as the machine file alone defines them and the report
    /// says why, because a checkout's broken file must not hide what the machine offers.
    /// The model rules are the machine file's alone, which is the only file that may set them.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Config`] when the machine's configuration cannot be read.
    pub fn quota(&self, vendor: Option<Vendor>) -> Result<QuotaReport, RunError> {
        let machine = Config::load_machine(&self.host_env)?;
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let (config, project_error) = match machine.clone().with_project(&self.host_env, &cwd) {
            Ok(config) => (config, None),
            Err(error) => (machine.clone(), Some(error)),
        };
        let vendors: &[Vendor] = vendor.as_ref().map_or(&Vendor::ALL, std::slice::from_ref);
        let accounts = vendors
            .iter()
            .flat_map(|vendor| {
                crate::quota::probe_vendor(*vendor, &config, &self.host_env, self.probe.as_ref())
            })
            .collect();
        Ok(QuotaReport {
            asked: vendor,
            accounts,
            project_error,
            machine,
        })
    }
}

/// What [`RunStore::quota`] found.
#[derive(Debug)]
pub struct QuotaReport {
    /// The vendor asked about, or `None` when both were.
    pub asked: Option<Vendor>,
    /// What each configured account of [`Self::vendors`] has left.
    pub accounts: Vec<crate::quota::AccountQuota>,
    /// Why the project file was passed over, when it could not be read.
    ///
    /// The accounts are then those the machine file alone defines.
    pub project_error: Option<ConfigError>,
    /// The machine file as read for this report.
    machine: Config,
}

impl QuotaReport {
    /// The vendors this report covers.
    #[must_use]
    pub fn vendors(&self) -> &[Vendor] {
        self.asked
            .as_ref()
            .map_or(&Vendor::ALL, std::slice::from_ref)
    }

    /// What the machine file lets a delegate of one vendor do, as read for this report; see
    /// [`Config::describe_permissions`].
    #[must_use]
    pub fn permissions(&self, vendor: Vendor) -> String {
        self.machine.describe_permissions(vendor)
    }

    /// The machine file's model rules for one vendor, as read for this report.
    #[must_use]
    pub fn model_rules(&self, vendor: Vendor) -> ModelRules<'_> {
        self.machine.model_rules(vendor)
    }
}
