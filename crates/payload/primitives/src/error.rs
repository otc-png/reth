//! Error types for payload operations.

use alloc::{boxed::Box, string::ToString};
use alloy_primitives::B256;
use alloy_rpc_types_engine::{ForkchoiceUpdateError, PayloadError, PayloadStatusEnum};
use core::error;
use reth_errors::{BlockExecutionError, ProviderError, RethError};
use tokio::sync::oneshot;

/// Possible error variants during payload building.
#[derive(Debug, thiserror::Error)]
pub enum PayloadBuilderError {
    /// Thrown when the parent header cannot be found
    #[error("missing parent header: {0}")]
    MissingParentHeader(B256),
    /// Thrown when the parent block is missing.
    #[error("missing parent block {0}")]
    MissingParentBlock(B256),
    /// An oneshot channels has been closed.
    #[error("sender has been dropped")]
    ChannelClosed,
    /// If there's no payload to resolve.
    #[error("missing payload")]
    MissingPayload,
    /// Other internal error
    #[error(transparent)]
    Internal(#[from] RethError),
    /// Unrecoverable error during evm execution.
    #[error("evm execution error: {0}")]
    EvmExecutionError(Box<dyn core::error::Error + Send + Sync>),
    /// Any other payload building errors.
    #[error(transparent)]
    Other(Box<dyn core::error::Error + Send + Sync>),
}

impl PayloadBuilderError {
    /// Create a new EVM error from a boxed error.
    pub fn evm<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::EvmExecutionError(Box::new(error))
    }

    /// Create a new error from a boxed error.
    pub fn other<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(error))
    }
}

impl From<ProviderError> for PayloadBuilderError {
    fn from(error: ProviderError) -> Self {
        Self::Internal(RethError::Provider(error))
    }
}

impl From<oneshot::error::RecvError> for PayloadBuilderError {
    fn from(_: oneshot::error::RecvError) -> Self {
        Self::ChannelClosed
    }
}

impl From<BlockExecutionError> for PayloadBuilderError {
    fn from(error: BlockExecutionError) -> Self {
        Self::evm(error)
    }
}

/// Thrown when the payload or attributes are known to be invalid __before__ processing.
///
/// This is used mainly for
/// [`validate_version_specific_fields`](crate::validate_version_specific_fields), which validates
/// both execution payloads and forkchoice update attributes with respect to a method version.
#[derive(thiserror::Error, Debug)]
pub enum EngineObjectValidationError {
    /// Thrown when the underlying validation error occurred while validating an
    /// `ExecutionPayload`.
    #[error("Payload validation error: {0}")]
    Payload(VersionSpecificValidationError),

    /// Thrown when the underlying validation error occurred while validating a
    /// `PayloadAttributes`.
    #[error("Payload attributes validation error: {0}")]
    PayloadAttributes(VersionSpecificValidationError),

    /// Thrown if `PayloadAttributes` or `ExecutionPayload` were provided with a timestamp, but the
    /// version of the engine method called is meant for a fork that occurs after the provided
    /// timestamp.
    #[error("Unsupported fork")]
    UnsupportedFork,
    /// Another type of error that is not covered by the above variants.
    #[error("Invalid params: {0}")]
    InvalidParams(#[from] Box<dyn core::error::Error + Send + Sync>),
}

/// Thrown when validating an execution payload OR payload attributes fails due to:
/// * The existence of a new field that is not supported in the given engine method version, or
/// * The absence of a field that is required in the given engine method version
#[derive(thiserror::Error, Debug)]
pub enum VersionSpecificValidationError {
    /// Thrown if the pre-V3 `PayloadAttributes` or `ExecutionPayload` contains a parent beacon
    /// block root
    #[error("parent beacon block root not supported before V3")]
    ParentBeaconBlockRootNotSupportedBeforeV3,
    /// Thrown if `engine_forkchoiceUpdatedV1` or `engine_newPayloadV1` contains withdrawals
    #[error("withdrawals not supported in V1")]
    WithdrawalsNotSupportedInV1,
    /// Thrown if `engine_forkchoiceUpdated` or `engine_newPayload` contains no withdrawals after
    /// Shanghai
    #[error("no withdrawals post-Shanghai")]
    NoWithdrawalsPostShanghai,
    /// Thrown if `engine_forkchoiceUpdated` or `engine_newPayload` contains withdrawals before
    /// Shanghai
    #[error("withdrawals pre-Shanghai")]
    HasWithdrawalsPreShanghai,
    /// Thrown if the `PayloadAttributes` or `ExecutionPayload` contains no parent beacon block
    /// root after Cancun
    #[error("no parent beacon block root post-cancun")]
    NoParentBeaconBlockRootPostCancun,
}

/// Error validating payload received over `newPayload` API.
#[derive(thiserror::Error, Debug)]
pub enum NewPayloadError {
    /// Payload validation error.
    #[error(transparent)]
    Eth(#[from] PayloadError),
    /// Custom payload validation error.
    #[error(transparent)]
    Other(Box<dyn error::Error + Send + Sync>),
}

impl NewPayloadError {
    /// Creates instance of variant [`NewPayloadError::Other`].
    #[inline]
    pub fn other(err: impl error::Error + Send + Sync + 'static) -> Self {
        Self::Other(Box::new(err))
    }
}

impl NewPayloadError {
    /// Returns `true` if the error is caused by a block hash mismatch.
    #[inline]
    pub const fn is_block_hash_mismatch(&self) -> bool {
        matches!(self, Self::Eth(PayloadError::BlockHash { .. }))
    }

    /// Returns `true` if the error is caused by invalid block hashes (Cancun).
    #[inline]
    pub const fn is_invalid_versioned_hashes(&self) -> bool {
        matches!(self, Self::Eth(PayloadError::InvalidVersionedHashes))
    }
}

impl From<NewPayloadError> for PayloadStatusEnum {
    fn from(error: NewPayloadError) -> Self {
        Self::Invalid { validation_error: error.to_string() }
    }
}

impl EngineObjectValidationError {
    /// Creates an instance of the `InvalidParams` variant with the given error.
    pub fn invalid_params<E>(error: E) -> Self
    where
        E: core::error::Error + Send + Sync + 'static,
    {
        Self::InvalidParams(Box::new(error))
    }
}

/// Thrown when validating the correctness of a payloadattributes object.
#[derive(thiserror::Error, Debug)]
pub enum InvalidPayloadAttributesError {
    /// Thrown if the timestamp of the payload attributes is invalid according to the engine specs.
    #[error("invalid timestamp")]
    InvalidTimestamp,
    /// Another type of error that is not covered by the above variants.
    #[error("Invalid params: {0}")]
    InvalidParams(#[from] Box<dyn core::error::Error + Send + Sync>),
}

impl From<InvalidPayloadAttributesError> for ForkchoiceUpdateError {
    fn from(_: InvalidPayloadAttributesError) -> Self {
        Self::UpdatedInvalidPayloadAttributes
    }
}
