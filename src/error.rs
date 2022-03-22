use kube::runtime::finalizer::Error as FinalizerError;
/// Implement a custom error type to propagate errors through our controller implementation.
/// NOTE(rossdylan): I would have just used anyhow, but kube-rs needs a `std::error::Error`
/// compatible error type as the result from reconcile
#[derive(thiserror::Error, Debug)]
pub(crate) enum Error {
    /// Errors returned from the trust-dns protocol library
    #[error("dns name conversion failure")]
    TrustDNSProto(#[from] trust_dns_proto::error::ProtoError),

    /// An error we kick back when we can't find the namespace of an object
    #[error("no namespace found in object")]
    NoNamespace,

    /// A pass through for kube-rs errors
    #[error("kube-rs failure")]
    Kube(#[from] kube::Error),

    #[error("finalizer failed to apply")]
    FinalizerApplyFailed {
        #[source]
        source: Box<Error>,
    },
    #[error("finalizer cleanup failed")]
    FinalizerCleanupFailed {
        #[source]
        source: Box<Error>,
    },
    #[error("failed to add finalizer")]
    FinalizerAddFailed {
        #[source]
        source: kube::Error,
    },
    #[error("failed to add finalizer")]
    FinalizerRemoveFailed {
        #[source]
        source: kube::Error,
    },
    #[error("unnamed object")]
    FinalizerUnnamedObject,
}

impl From<FinalizerError<Self>> for Error {
    fn from(error: FinalizerError<Self>) -> Self {
        match error {
            FinalizerError::ApplyFailed(inner) => Self::FinalizerApplyFailed {
                source: Box::new(inner),
            },
            FinalizerError::CleanupFailed(inner) => Self::FinalizerCleanupFailed {
                source: Box::new(inner),
            },
            FinalizerError::AddFinalizer(inner) => Self::FinalizerAddFailed { source: inner },
            FinalizerError::RemoveFinalizer(inner) => Self::FinalizerRemoveFailed { source: inner },
            FinalizerError::UnnamedObject => Self::FinalizerUnnamedObject,
        }
    }
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
