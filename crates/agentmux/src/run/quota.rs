//! The usage windows a consultation reports.

use std::path::PathBuf;

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
        let _ = write_json(&path, &reported);
        reported
    }

    /// Ask each configured account what it has left, for the given vendors.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Config`] when the machine's configuration cannot be read.
    pub fn quota(
        &self,
        vendor: Option<Vendor>,
    ) -> Result<Vec<crate::quota::AccountQuota>, RunError> {
        // Discovered from agentmux's own directory: a quota question is about this machine, not
        // about any one consultation.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = crate::config::Config::load(&self.host_env, &cwd)?;
        let vendors: &[Vendor] = vendor.as_ref().map_or(&Vendor::ALL, std::slice::from_ref);
        Ok(vendors
            .iter()
            .flat_map(|vendor| {
                crate::quota::probe_vendor(*vendor, &config, &self.host_env, self.probe.as_ref())
            })
            .collect())
    }
}
