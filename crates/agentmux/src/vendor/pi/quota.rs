//! What pi's login has left, which pi does not say.

use crate::quota::Observation;

/// Report pi's quota as unavailable: pi has no command that reports a provider's usage window,
/// and the providers it reaches are the user's to choose, so there is no one window to read.
pub(crate) fn probe() -> Observation {
    Observation::Unavailable {
        reason: "pi reports no usage window for the providers it reaches".to_owned(),
    }
}
