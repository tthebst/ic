//! This module provides the component responsible for generating and validating
//! payloads relevant to canister threshold signatures.
//!
//! # Goal
//! We want canisters to be able to hold tokens of other chains, i.e. BTC, ETH, SOL,
//! and for them to create transactions for these networks, i.e. bitcoin, ethereum,
//! solana. Since those networks use specific signature schemes such as ECDSA or Schnorr,
//! a canister must be able to create signatures according to these schemes. Since a
//! canister cannot hold the secret key itself, the secret key will be shared among the
//! replicas of the subnet, and they must be able to collaboratively create threshold
//! signatures.
//!
//! # High level implementation design
//! Each subnet will have a single threshold key for each scheme deployed to the subnet.
//! Currently, only threshold ECDSA and threshold Schnorr are supported. From this key,
//! we will derive per-canister keys. A canister can via a system API request a signature,
//! and this request is stored in the replicated state. Consensus will observe these
//! requests and begin working on them by assembling required artifacts in blocks.
//!
//! ## Interactive Distributed Key Generation & Transcripts
//! To create canister threshold signatures we need a `Transcript` that gives all
//! replicas shares of a secret key. However, this is not sufficient: we need additional
//! transcripts to share the ephemeral values used in a signature.
//!
//! The creation of one ECDSA signature requires a transcript that
//! shares the ECDSA signing key `x`, and additionally four IDKG transcripts,
//! with a special structure: we need transcripts `t1`, `t2`, `t3`, `t4`, such
//! that `t1` and `t2` share random values `r1` and `r2` respectively, `t3`
//! shares the product `r1 * r2`, and `t4` shares `r2 * x`.
//!
//! Similarly, the creation of one Schnorr signature requires a transcript that
//! shares the Schnorr signing key `x`, and one additional IDKG transcript (blinder) `t`,
//! such that `t` shares a random value `r`.
//!
//! Such transcripts are created via an interactive distributed key generation (IDKG)
//! protocol. Especially for the ECDSA case, the DKG for these transcripts must be
//! computationally efficient, because we need four transcripts per signature, and we
//! want to be able to create many signatures. This means that we need interactive DKG
//! for canister threshold signatures, instead of non-interactive DKG like we do for
//! our threshold BLS signatures.
//!
//! Consensus orchestrates the creation of these transcripts. Blocks contain
//! configs (also called params) indicating which transcripts should be created.
//! Such configs come in different types, because some transcripts should share a
//! random value, while others need to share the product of two other transcripts.
//! Complete transcripts will be included in blocks via the functions
//! `create_data_payload` and `create_summary_payload`.
//!
//! # [IDkgImpl] behavior
//! The IDKG component is responsible for adding artifacts to the IDKG
//! artifact pool, and validating artifacts in that pool, by exposing a function
//! `on_state_change`. This function behaves as follows, where `finalized_tip`
//! denotes the latest finalized consensus block, and `certified_state` denotes
//! the latest certified state.
//!
//! ## add DKG dealings
//! for every config in `finalized_tip.idkg.configs`, do the following: if this
//! replica is a dealer in this config, and no dealing for this config created
//! by this replica is in the validated pool, attempt to load the dependencies and,
//! if successful, create a dealing for this config, and add it to the validated pool.
//! If loading the dependencies (i.e. t3 depends on t2 and t1) wasn't successful,
//! we instead send a complaint for the transcript that failed to load.
//!
//! ## validate IDKG dealings
//! for every unvalidated dealing d, do the following. If `d.config_id` is an
//! element of `finalized_tip.idkg.configs`, the validated pool does not yet
//! contain a dealing from `d.dealer` for `d.config_id`, then do the public
//! cryptographic validation of the dealing, and move it to the validated pool
//! if valid, or remove it from the unvalidated pool if invalid.
//!
//! ## Support DKG dealings
//! In the previous step, we only did the "public" verification of the dealings,
//! which does not check that the dealing encrypts a good share for this
//! replica. For every validated dealing d for which no support message by this
//! replica exists in the validated pool, do the "private" cryptographic
//! validation, and if valid, add a support dealing message for d to the
//! validated pool.
//!
//! ## Remove stale dealings
//! for every validated or unvalidated dealing or support d, do the following.
//! If `d.config_id` is not an element of `finalized_tip.idkg.configs`, and
//! `d.config_id` is older than `finalized_tip`, remove `d` from the pool.
//!
//! ## add signature shares
//! for every signature request `req` in
//! `certified_state.signature_requests`, do the following: if this replica
//! is a signer for `req` and no signature share by this replica is in the
//! validated pool, load the dependencies (i.e. the pre-signature and key transcripts),
//! then create a signature share for `req` and add it to the validated pool.
//!
//! ## validate signature shares
//! for every unvalidated signature share `s`, do the following: if `s.request_id`
//! is an element of `certified_state.signature_requests`, and there is no signature
//! share by `s.signer` for `s.request_id` in the validated pool yet, then load the
//! dependencies and cryptographically validate the signature share. If valid, move
//! `s` to validated, and if invalid, remove `s` from unvalidated.
//!
//! ## aggregate signature shares
//! Signature shares are aggregated into full signatures and included into a block
//! by the block maker, once enough shares are available.
//!
//! ## validate complaints
//! for every unvalidated complaint `c`, do the following: if `c.config_id`
//! is an element of `finalized_tip.idkg.configs`, and there is no complaint
//! by `c.complainer` for `c.config_id` in the validated pool yet, then
//! cryptographically validate the signature of the complaint and the complaint
//! itself. If valid, move `c` to validated, and if invalid, remove `c` from unvalidated.
//!
//! ## send openings
//! for every validated complaint `c` for which this node has not sent an opening yet and
//! for which `c.config_id` is an element of `finalized_tip.idkg.configs`: create and sign
//! the opening, and add it to the validated pool.
//!
//!
//! # IDKG payload on blocks
//! The IDKG payload on blocks serves some purposes: it should ensure that all
//! replicas are doing IDKGs to help create the transcripts required for more
//! pre-signatures which are used to create threshold signatures. Additionally, it
//! should contain newly aggregated signatures that can be delivered back to execution.
//!
//! Every block contains
//! - a set of "pre-signatures being created"
//! - a set of "available pre-signatures"
//! - newly finished signatures to deliver up
//!
//! The ECDSA "pre-signatures in creation" contain the following information
//! - kappa_config: config for 1st unmasked random transcript
//! - optionally, kappa_unmasked: transcript resulting from kappa_config
//! - lambda_config: config for 2nd masked random transcript
//! - optionally, lambda_masked: transcript resulting from lambda_config
//! - optionally, key_times_lambda_config: multiplication of the ECDSA secret
//!   key and lambda_masked transcript (so masked multiplication of unmasked and
//!   masked)
//! - optionally, key_times_lambda: transcript resulting from
//!   key_times_lambda_config
//! - optionally, kappa_times_lambda_config: config of multiplication
//!   kappa_unmasked and lambda_masked (so masked multiplication of unmasked and
//!   masked)
//! - optionally, kappa_times_lambda: transcript resulting from
//!   kappa_times_lambda_config
//!
//! The relation between the different configs/transcripts can be summarized as
//! follows:
//! ```text
//! kappa_unmasked ─────────►
//!                           kappa_times_lambda
//!         ┌───────────────►
//!         │
//! lambda_masked
//!         │
//!         └───────────────►
//!                           key_times_lambda
//! ecdsa_key  ─────────────►
//! ```
//! The data transforms like a state machine:
//! - when a new transcript is complete, it is added to the corresponding
//!   "4-tuple being created"
//!     - when lambda_masked is set, key_times_lambda_config should be set
//!     - when lambda_masked and kappa_unmasked are set,
//!       kappa_times_lambda_config must be set
//!     - when kappa_unmasked, lambda_masked, key_times_lambda,
//!       kappa_times_lambda are set, the tuple should no longer be in "in
//!       creation", but instead be moved to the complete 4-tuples.
//!
//! //! The Schnorr "pre-signatures in creation" contain the following information
//! - blinder_config: config for unmasked random transcript
//! - optionally, blinder_unmasked: transcript resulting from blinder_config
//!
//! The relation between the different configs/transcripts can be summarized as
//! follows:
//! ```text
//! blinder_unmasked
//! ```
//! The data transforms like a state machine:
//! - when a new transcript is complete, it is added to the corresponding
//!   "pre-signature being created"
//!     - when blinder_unmasked is set, the pre-signature should no longer be in "in
//!       creation", but instead be moved to the complete pre-signatures.
//!
//! Completed pre-signatures are delivered to the deterministic state machnine,
//! where they are matched with incoming signature requests.

use crate::idkg::complaints::{IDkgComplaintHandler, IDkgComplaintHandlerImpl};
use crate::idkg::metrics::{
    timed_call, IDkgClientMetrics, IDkgGossipMetrics,
    CRITICAL_ERROR_ECDSA_RETAIN_ACTIVE_TRANSCRIPTS,
};
use crate::idkg::pre_signer::{IDkgPreSigner, IDkgPreSignerImpl};
use crate::idkg::signer::{ThresholdSigner, ThresholdSignerImpl};
use crate::idkg::utils::IDkgBlockReaderImpl;

use ic_consensus_utils::crypto::ConsensusCrypto;
use ic_consensus_utils::RoundRobin;
use ic_interfaces::{
    consensus_pool::ConsensusBlockCache,
    crypto::IDkgProtocol,
    idkg::{IDkgChangeSet, IDkgPool},
    p2p::consensus::{ChangeSetProducer, Priority, PriorityFn, PriorityFnFactory},
};
use ic_interfaces_state_manager::StateReader;
use ic_logger::{error, warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_replicated_state::ReplicatedState;
use ic_types::consensus::idkg::IDkgMessage;
use ic_types::crypto::canister_threshold_sig::error::IDkgRetainKeysError;
use ic_types::{
    artifact::IDkgMessageId,
    consensus::idkg::{IDkgBlockReader, IDkgMessageAttribute, RequestId},
    crypto::canister_threshold_sig::idkg::IDkgTranscriptId,
    malicious_flags::MaliciousFlags,
    Height, NodeId, SubnetId,
};

use std::cell::RefCell;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) mod complaints;
#[cfg(any(feature = "malicious_code", test))]
pub mod malicious_pre_signer;
pub(crate) mod metrics;
pub(crate) mod payload_builder;
pub(crate) mod payload_verifier;
pub(crate) mod pre_signer;
pub(crate) mod signer;
pub mod stats;
#[cfg(test)]
pub(crate) mod test_utils;
pub(crate) mod utils;

pub(crate) use payload_builder::{
    create_data_payload, create_summary_payload, make_bootstrap_summary,
};
pub(crate) use payload_verifier::{
    validate_payload, IDkgPayloadValidationFailure, InvalidIDkgPayloadReason,
};
pub use stats::IDkgStatsImpl;

use self::utils::get_context_request_id;

/// Similar to consensus, we don't fetch artifacts too far ahead in future.
const LOOK_AHEAD: u64 = 10;

/// Frequency for clearing the inactive key transcripts.
pub(crate) const INACTIVE_TRANSCRIPT_PURGE_SECS: Duration = Duration::from_secs(60);

/// `IDkgImpl` is the consensus component responsible for processing threshold
/// IDKG payloads.
pub struct IDkgImpl {
    /// The Pre-Signer subcomponent
    pub pre_signer: Box<IDkgPreSignerImpl>,
    signer: Box<dyn ThresholdSigner>,
    complaint_handler: Box<dyn IDkgComplaintHandler>,
    consensus_block_cache: Arc<dyn ConsensusBlockCache>,
    crypto: Arc<dyn ConsensusCrypto>,
    schedule: RoundRobin,
    last_transcript_purge_ts: RefCell<Instant>,
    metrics: IDkgClientMetrics,
    logger: ReplicaLogger,
    #[cfg_attr(not(feature = "malicious_code"), allow(dead_code))]
    malicious_flags: MaliciousFlags,
}

impl IDkgImpl {
    /// Builds a new IDKG component
    pub fn new(
        node_id: NodeId,
        consensus_block_cache: Arc<dyn ConsensusBlockCache>,
        crypto: Arc<dyn ConsensusCrypto>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        metrics_registry: MetricsRegistry,
        logger: ReplicaLogger,
        malicious_flags: MaliciousFlags,
    ) -> Self {
        let pre_signer = Box::new(IDkgPreSignerImpl::new(
            node_id,
            consensus_block_cache.clone(),
            crypto.clone(),
            metrics_registry.clone(),
            logger.clone(),
        ));
        let signer = Box::new(ThresholdSignerImpl::new(
            node_id,
            consensus_block_cache.clone(),
            crypto.clone(),
            state_reader,
            metrics_registry.clone(),
            logger.clone(),
        ));
        let complaint_handler = Box::new(IDkgComplaintHandlerImpl::new(
            node_id,
            consensus_block_cache.clone(),
            crypto.clone(),
            metrics_registry.clone(),
            logger.clone(),
        ));
        Self {
            pre_signer,
            signer,
            complaint_handler,
            crypto,
            consensus_block_cache,
            schedule: RoundRobin::default(),
            last_transcript_purge_ts: RefCell::new(Instant::now()),
            metrics: IDkgClientMetrics::new(metrics_registry),
            logger,
            malicious_flags,
        }
    }

    /// Purges the transcripts that are no longer active.
    fn purge_inactive_transcripts(&self, block_reader: &dyn IDkgBlockReader) {
        let mut active_transcripts = HashSet::new();
        let mut error_count = 0;
        for transcript_ref in block_reader.active_transcripts() {
            match block_reader.transcript(&transcript_ref) {
                Ok(transcript) => {
                    self.metrics
                        .client_metrics
                        .with_label_values(&["resolve_active_transcript_refs"])
                        .inc();
                    active_transcripts.insert(transcript);
                }
                Err(error) => {
                    warn!(
                        self.logger,
                        "purge_inactive_transcripts(): failed to resolve transcript ref: err = {:?}, \
                        {:?}",
                        error,
                        transcript_ref,
                    );
                    self.metrics
                        .client_errors
                        .with_label_values(&["resolve_active_transcript_refs"])
                        .inc();
                    error_count += 1;
                }
            }
        }

        if error_count > 0 {
            warn!(
                self.logger,
                "purge_inactive_transcripts(): abort due to {} errors", error_count,
            );
            return;
        }

        match IDkgProtocol::retain_active_transcripts(&*self.crypto, &active_transcripts) {
            Err(IDkgRetainKeysError::TransientInternalError { internal_error }) => {
                warn!(
                    self.logger,
                    "purge_inactive_transcripts(): failed due to transient error: {}",
                    internal_error
                );
                self.metrics
                    .client_errors
                    .with_label_values(&["retain_active_transcripts_transient"])
                    .inc();
            }
            Err(error) => {
                error!(
                    self.logger,
                    "{}: failed with error = {:?}",
                    CRITICAL_ERROR_ECDSA_RETAIN_ACTIVE_TRANSCRIPTS,
                    error
                );
                self.metrics
                    .critical_error_ecdsa_retain_active_transcripts
                    .inc();
            }
            Ok(()) => {
                self.metrics
                    .client_metrics
                    .with_label_values(&["retain_active_transcripts"])
                    .inc();
            }
        }
    }
}

impl<T: IDkgPool> ChangeSetProducer<T> for IDkgImpl {
    type ChangeSet = IDkgChangeSet;

    fn on_state_change(&self, idkg_pool: &T) -> IDkgChangeSet {
        let metrics = self.metrics.clone();
        let pre_signer = || {
            let changeset = timed_call(
                "pre_signer",
                || {
                    self.pre_signer
                        .on_state_change(idkg_pool, self.complaint_handler.as_transcript_loader())
                },
                &metrics.on_state_change_duration,
            );
            #[cfg(any(feature = "malicious_code", test))]
            if self.malicious_flags.is_idkg_malicious() {
                return super::idkg::malicious_pre_signer::maliciously_alter_changeset(
                    changeset,
                    &self.pre_signer,
                    &self.malicious_flags,
                );
            }
            changeset
        };
        let signer = || {
            timed_call(
                "signer",
                || {
                    self.signer
                        .on_state_change(idkg_pool, self.complaint_handler.as_transcript_loader())
                },
                &metrics.on_state_change_duration,
            )
        };
        let complaint_handler = || {
            timed_call(
                "complaint_handler",
                || self.complaint_handler.on_state_change(idkg_pool),
                &metrics.on_state_change_duration,
            )
        };

        let calls: [&'_ dyn Fn() -> IDkgChangeSet; 3] = [&pre_signer, &signer, &complaint_handler];
        let ret = self.schedule.call_next(&calls);

        if self.last_transcript_purge_ts.borrow().elapsed() >= INACTIVE_TRANSCRIPT_PURGE_SECS {
            let block_reader =
                IDkgBlockReaderImpl::new(self.consensus_block_cache.finalized_chain());
            timed_call(
                "purge_inactive_transcripts",
                || self.purge_inactive_transcripts(&block_reader),
                &metrics.on_state_change_duration,
            );
            *self.last_transcript_purge_ts.borrow_mut() = Instant::now();
        }
        ret
    }
}

/// `IDkgGossipImpl` implements the priority function and other gossip related
/// functionality
pub struct IDkgGossipImpl {
    subnet_id: SubnetId,
    consensus_block_cache: Arc<dyn ConsensusBlockCache>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    metrics: IDkgGossipMetrics,
}

impl IDkgGossipImpl {
    /// Builds a new IDkgGossipImpl component
    pub fn new(
        subnet_id: SubnetId,
        consensus_block_cache: Arc<dyn ConsensusBlockCache>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        metrics_registry: MetricsRegistry,
    ) -> Self {
        Self {
            subnet_id,
            consensus_block_cache,
            state_reader,
            metrics: IDkgGossipMetrics::new(metrics_registry),
        }
    }
}

struct IDkgPriorityFnArgs {
    finalized_height: Height,
    #[allow(dead_code)]
    certified_height: Height,
    requested_transcripts: BTreeSet<IDkgTranscriptId>,
    requested_signatures: BTreeSet<RequestId>,
    active_transcripts: BTreeSet<IDkgTranscriptId>,
}

impl IDkgPriorityFnArgs {
    fn new(
        block_reader: &dyn IDkgBlockReader,
        state_reader: &dyn StateReader<State = ReplicatedState>,
    ) -> Self {
        let mut requested_transcripts = BTreeSet::new();
        for params in block_reader.requested_transcripts() {
            requested_transcripts.insert(params.transcript_id);
        }

        let mut active_transcripts = BTreeSet::new();
        for transcript_ref in block_reader.active_transcripts() {
            active_transcripts.insert(transcript_ref.transcript_id);
        }

        let (certified_height, requested_signatures) = state_reader
            .get_certified_state_snapshot()
            .map_or(Default::default(), |snapshot| {
                let request_contexts = snapshot
                    .get_state()
                    .signature_request_contexts()
                    .values()
                    .flat_map(get_context_request_id)
                    .collect::<BTreeSet<_>>();

                (snapshot.get_height(), request_contexts)
            });

        Self {
            finalized_height: block_reader.tip_height(),
            certified_height,
            requested_transcripts,
            requested_signatures,
            active_transcripts,
        }
    }
}

impl<Pool: IDkgPool> PriorityFnFactory<IDkgMessage, Pool> for IDkgGossipImpl {
    fn get_priority_function(
        &self,
        _idkg_pool: &Pool,
    ) -> PriorityFn<IDkgMessageId, IDkgMessageAttribute> {
        let block_reader = IDkgBlockReaderImpl::new(self.consensus_block_cache.finalized_chain());
        let subnet_id = self.subnet_id;
        let args = IDkgPriorityFnArgs::new(&block_reader, self.state_reader.as_ref());
        let metrics = self.metrics.clone();
        Box::new(move |_, attr: &'_ IDkgMessageAttribute| {
            compute_priority(attr, subnet_id, &args, &metrics)
        })
    }
}

fn compute_priority(
    attr: &IDkgMessageAttribute,
    subnet_id: SubnetId,
    args: &IDkgPriorityFnArgs,
    metrics: &IDkgGossipMetrics,
) -> Priority {
    match attr {
        IDkgMessageAttribute::Dealing(transcript_id)
        | IDkgMessageAttribute::DealingSupport(transcript_id) => {
            // For xnet dealings(target side), always fetch the artifacts,
            // as the source_height from different subnet cannot be compared
            // anyways.
            if *transcript_id.source_subnet() != subnet_id {
                return Priority::FetchNow;
            }

            let height = transcript_id.source_height();
            if height <= args.finalized_height {
                if args.requested_transcripts.contains(transcript_id) {
                    Priority::FetchNow
                } else {
                    metrics
                        .dropped_adverts
                        .with_label_values(&[attr.as_str()])
                        .inc();
                    Priority::Drop
                }
            } else if height < args.finalized_height + Height::from(LOOK_AHEAD) {
                Priority::FetchNow
            } else {
                Priority::Stash
            }
        }
        IDkgMessageAttribute::EcdsaSigShare(request_id)
        | IDkgMessageAttribute::SchnorrSigShare(request_id) => {
            if request_id.height <= args.certified_height {
                if args.requested_signatures.contains(request_id) {
                    Priority::FetchNow
                } else {
                    metrics
                        .dropped_adverts
                        .with_label_values(&[attr.as_str()])
                        .inc();
                    Priority::Drop
                }
            } else if request_id.height < args.certified_height + Height::from(LOOK_AHEAD) {
                Priority::FetchNow
            } else {
                Priority::Stash
            }
        }
        IDkgMessageAttribute::Complaint(transcript_id)
        | IDkgMessageAttribute::Opening(transcript_id) => {
            let height = transcript_id.source_height();
            if height <= args.finalized_height {
                if args.active_transcripts.contains(transcript_id)
                    || args.requested_transcripts.contains(transcript_id)
                {
                    Priority::FetchNow
                } else {
                    metrics
                        .dropped_adverts
                        .with_label_values(&[attr.as_str()])
                        .inc();
                    Priority::Drop
                }
            } else if height < args.finalized_height + Height::from(LOOK_AHEAD) {
                Priority::FetchNow
            } else {
                Priority::Stash
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use self::test_utils::{
        fake_completed_signature_request_context, fake_signature_request_context_with_pre_sig,
        fake_state_with_signature_requests, TestIDkgBlockReader,
    };

    use super::*;
    use ic_test_utilities::state_manager::RefMockStateManager;
    use ic_types::consensus::idkg::{IDkgUIDGenerator, PreSigId};
    use ic_types::crypto::canister_threshold_sig::idkg::IDkgTranscriptId;
    use ic_types::{consensus::idkg::RequestId, PrincipalId, SubnetId};
    use test_utils::fake_ecdsa_master_public_key_id;
    use tests::test_utils::create_sig_inputs;

    #[test]
    fn test_idkg_priority_fn_args() {
        let state_manager = Arc::new(RefMockStateManager::default());
        let height = Height::from(100);
        let key_id = fake_ecdsa_master_public_key_id();
        // Add two contexts to state, one with, and one without quadruple
        let pre_sig_id = PreSigId(0);
        let context_with_quadruple =
            fake_completed_signature_request_context(0, key_id.clone(), pre_sig_id);
        let context_without_quadruple =
            fake_signature_request_context_with_pre_sig(1, key_id.clone(), None);
        let snapshot = fake_state_with_signature_requests(
            height,
            [
                context_with_quadruple.clone(),
                context_without_quadruple.clone(),
            ],
        );
        state_manager
            .get_mut()
            .expect_get_certified_state_snapshot()
            .returning(move || Some(Box::new(snapshot.clone()) as Box<_>));

        let expected_request_id = get_context_request_id(&context_with_quadruple.1).unwrap();
        assert_eq!(expected_request_id.pseudo_random_id, [0; 32]);
        assert_eq!(expected_request_id.pre_signature_id, pre_sig_id);

        let block_reader = TestIDkgBlockReader::for_signer_test(
            height,
            vec![(expected_request_id.clone(), create_sig_inputs(0, &key_id))],
        );

        // Only the context with matched quadruple should be in "requested"
        let args = IDkgPriorityFnArgs::new(&block_reader, state_manager.as_ref());
        assert_eq!(args.certified_height, height);
        assert_eq!(args.requested_signatures.len(), 1);
        assert_eq!(
            args.requested_signatures.first().unwrap(),
            &expected_request_id
        );
    }

    // Tests the priority computation for dealings/support.
    #[test]
    fn test_idkg_priority_fn_dealing_support() {
        let xnet_subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(1));
        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(2));
        let xnet_transcript_id = IDkgTranscriptId::new(xnet_subnet_id, 1, Height::from(1000));
        let transcript_id_fetch_1 = IDkgTranscriptId::new(subnet_id, 1, Height::from(80));
        let transcript_id_drop = IDkgTranscriptId::new(subnet_id, 2, Height::from(70));
        let transcript_id_fetch_2 = IDkgTranscriptId::new(subnet_id, 3, Height::from(102));
        let transcript_id_stash = IDkgTranscriptId::new(subnet_id, 4, Height::from(200));

        let metrics_registry = MetricsRegistry::new();
        let metrics = IDkgGossipMetrics::new(metrics_registry);

        let mut requested_transcripts = BTreeSet::new();
        requested_transcripts.insert(transcript_id_fetch_1);
        let args = IDkgPriorityFnArgs {
            finalized_height: Height::from(100),
            certified_height: Height::from(100),
            requested_transcripts,
            requested_signatures: BTreeSet::new(),
            active_transcripts: BTreeSet::new(),
        };

        let tests = vec![
            // Signed dealings
            (
                IDkgMessageAttribute::Dealing(xnet_transcript_id),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Dealing(transcript_id_fetch_1),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Dealing(transcript_id_drop),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::Dealing(transcript_id_fetch_2),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Dealing(transcript_id_stash),
                Priority::Stash,
            ),
            // Dealing support
            (
                IDkgMessageAttribute::DealingSupport(xnet_transcript_id),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::DealingSupport(transcript_id_fetch_1),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::DealingSupport(transcript_id_drop),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::DealingSupport(transcript_id_fetch_2),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::DealingSupport(transcript_id_stash),
                Priority::Stash,
            ),
        ];

        for (attr, expected) in tests {
            assert_eq!(
                compute_priority(&attr, subnet_id, &args, &metrics),
                expected
            );
        }
    }

    // Tests the priority computation for sig shares.
    #[test]
    fn test_idkg_priority_fn_sig_shares() {
        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(2));
        let mut uid_generator = IDkgUIDGenerator::new(subnet_id, Height::new(0));
        let request_id_fetch_1 = RequestId {
            pre_signature_id: uid_generator.next_pre_signature_id(),
            pseudo_random_id: [1; 32],
            height: Height::from(80),
        };
        let request_id_drop = RequestId {
            pre_signature_id: uid_generator.next_pre_signature_id(),
            pseudo_random_id: [2; 32],
            height: Height::from(70),
        };
        let request_id_fetch_2 = RequestId {
            pre_signature_id: uid_generator.next_pre_signature_id(),
            pseudo_random_id: [3; 32],
            height: Height::from(102),
        };
        let request_id_stash = RequestId {
            pre_signature_id: uid_generator.next_pre_signature_id(),
            pseudo_random_id: [4; 32],
            height: Height::from(200),
        };

        let metrics_registry = MetricsRegistry::new();
        let metrics = IDkgGossipMetrics::new(metrics_registry);

        let mut requested_signatures = BTreeSet::new();
        requested_signatures.insert(request_id_fetch_1.clone());
        let args = IDkgPriorityFnArgs {
            finalized_height: Height::from(100),
            certified_height: Height::from(100),
            requested_transcripts: BTreeSet::new(),
            requested_signatures,
            active_transcripts: BTreeSet::new(),
        };

        let tests = vec![
            (
                IDkgMessageAttribute::EcdsaSigShare(request_id_fetch_1.clone()),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::SchnorrSigShare(request_id_fetch_1.clone()),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::EcdsaSigShare(request_id_drop.clone()),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::SchnorrSigShare(request_id_drop.clone()),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::EcdsaSigShare(request_id_fetch_2.clone()),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::SchnorrSigShare(request_id_fetch_2.clone()),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::EcdsaSigShare(request_id_stash.clone()),
                Priority::Stash,
            ),
            (
                IDkgMessageAttribute::SchnorrSigShare(request_id_stash.clone()),
                Priority::Stash,
            ),
        ];

        for (attr, expected) in tests {
            assert_eq!(
                compute_priority(&attr, subnet_id, &args, &metrics),
                expected
            );
        }
    }

    // Tests the priority computation for complaints/openings.
    #[test]
    fn test_idkg_priority_fn_complaint_opening() {
        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(2));
        let transcript_id_fetch_1 = IDkgTranscriptId::new(subnet_id, 1, Height::from(80));
        let transcript_id_drop = IDkgTranscriptId::new(subnet_id, 2, Height::from(70));
        let transcript_id_fetch_2 = IDkgTranscriptId::new(subnet_id, 3, Height::from(102));
        let transcript_id_stash = IDkgTranscriptId::new(subnet_id, 4, Height::from(200));
        let transcript_id_fetch_3 = IDkgTranscriptId::new(subnet_id, 5, Height::from(80));

        let metrics_registry = MetricsRegistry::new();
        let metrics = IDkgGossipMetrics::new(metrics_registry);

        let mut active_transcripts = BTreeSet::new();
        active_transcripts.insert(transcript_id_fetch_1);
        let mut requested_transcripts = BTreeSet::new();
        requested_transcripts.insert(transcript_id_fetch_3);
        let args = IDkgPriorityFnArgs {
            finalized_height: Height::from(100),
            certified_height: Height::from(100),
            requested_transcripts,
            requested_signatures: BTreeSet::new(),
            active_transcripts,
        };

        let tests = vec![
            // Complaints
            (
                IDkgMessageAttribute::Complaint(transcript_id_fetch_1),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Complaint(transcript_id_drop),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::Complaint(transcript_id_fetch_2),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Complaint(transcript_id_stash),
                Priority::Stash,
            ),
            (
                IDkgMessageAttribute::Complaint(transcript_id_fetch_3),
                Priority::FetchNow,
            ),
            // Openings
            (
                IDkgMessageAttribute::Opening(transcript_id_fetch_1),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Opening(transcript_id_drop),
                Priority::Drop,
            ),
            (
                IDkgMessageAttribute::Opening(transcript_id_fetch_2),
                Priority::FetchNow,
            ),
            (
                IDkgMessageAttribute::Opening(transcript_id_stash),
                Priority::Stash,
            ),
            (
                IDkgMessageAttribute::Opening(transcript_id_fetch_3),
                Priority::FetchNow,
            ),
        ];

        for (attr, expected) in tests {
            assert_eq!(
                compute_priority(&attr, subnet_id, &args, &metrics),
                expected
            );
        }
    }
}
