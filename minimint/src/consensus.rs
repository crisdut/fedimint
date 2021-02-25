use crate::database::{
    Database, DatabaseDecode, DatabaseEncode, DatabaseError, DecodingError, PrefixSearchable,
    Transactional,
};
use crate::net::api::ClientRequest;
use crate::rng::RngGenerator;
use config::ServerConfig;
use fedimint::Mint;
use hbbft::honey_badger::Batch;
use mint_api::{Coin, PartialSigResponse, PegInRequest, ReissuanceRequest, RequestId, SigResponse};
use musig;
use rand::{CryptoRng, RngCore};
use serde::{Deserialize, Serialize};
use sled::IVec;
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum ConsensusItem {
    ClientRequest(ClientRequest),
    PartiallySignedRequest(mint_api::PartialSigResponse),
}

pub type HoneyBadgerMessage = hbbft::honey_badger::Message<u16>;

pub struct FediMintConsensus<R, D>
where
    R: RngCore + CryptoRng,
    D: Database + PrefixSearchable + Transactional,
{
    /// Cryptographic random number generator used for everything
    pub rng_gen: Box<dyn RngGenerator<Rng = R>>,
    /// Configuration describing the federation and containing our secrets
    pub cfg: ServerConfig, // TODO: make custom config

    /// Our local mint
    pub mint: Mint, //TODO: box dyn trait for testability

    /// KV Database into which all state is persisted to recover from in case of a crash
    pub db: D,
}

impl<R, D> FediMintConsensus<R, D>
where
    R: RngCore + CryptoRng,
    D: Database + PrefixSearchable + Transactional,
{
    pub fn submit_client_request(&mut self, cr: ClientRequest) -> Result<(), ClientRequestError> {
        debug!("Received client request of type {}", cr.dbg_type_name());
        match cr {
            ClientRequest::Reissuance(ref reissuance_req) => {
                let pub_keys = reissuance_req
                    .coins
                    .iter()
                    .map(Coin::spend_key)
                    .collect::<Vec<_>>();

                if !musig::verify(
                    reissuance_req.digest(),
                    reissuance_req.sig.clone(),
                    &pub_keys,
                ) {
                    warn!("Rejecting invalid reissuance request: invalid tx sig");
                    return Err(ClientRequestError::InvalidTransactionSignature);
                }

                if !self.mint.validate(&reissuance_req.coins) {
                    warn!("Rejecting invalid reissuance request: spent or invalid mint sig");
                    return Err(ClientRequestError::DeniedByMint);
                }
            }
            _ => {
                // FIXME: validate other request types or move validation elsewhere
            }
        }

        let new = self
            .db
            .insert_entry(&ConsensusItem::ClientRequest(cr), &())
            .expect("DB error");

        if new.is_some() {
            warn!("Added consensus item was already in consensus queue");
        }

        Ok(())
    }

    pub fn process_consensus_outcome(
        &mut self,
        batch: Batch<Vec<ConsensusItem>, u16>,
    ) -> Vec<SigResponse> {
        info!("Processing output of epoch {}", batch.epoch);

        let mut signaturre_responses = Vec::new();

        for (peer, ci) in batch.contributions.into_iter().flat_map(|(peer, cis)| {
            debug!("Peer {} contributed {} items", peer, cis.len());
            cis.into_iter().map(move |ci| (peer, ci))
        }) {
            trace!("Processing consensus item {:?} from peer {}", ci, peer);
            self.db.remove_entry::<_, ()>(&ci).expect("DB error");

            match ci {
                ConsensusItem::ClientRequest(client_request) => {
                    self.process_client_request(peer, client_request)
                }
                ConsensusItem::PartiallySignedRequest(psig) => {
                    if let Some(signature_response) = self.process_partial_signature(peer, psig) {
                        signaturre_responses.push(signature_response);
                    }
                }
            };
        }

        signaturre_responses
    }

    pub fn get_consensus_proposal(&mut self) -> Vec<ConsensusItem> {
        self.db
            .find_by_prefix(&ConsensusItemKeyPrefix)
            .map(|res| res.map(|(ci, ())| ci))
            .collect::<Result<_, DatabaseError>>()
            .expect("DB error")
    }

    fn process_client_request(&mut self, peer: u16, cr: ClientRequest) {
        match cr {
            ClientRequest::PegIn(peg_in) => self.process_peg_in_request(peg_in),
            ClientRequest::Reissuance(reissuance) => {
                self.process_reissuance_request(peer, reissuance)
            }
            ClientRequest::PegOut(_req) => {
                unimplemented!()
            }
        };
    }

    fn process_peg_in_request(&mut self, peg_in: PegInRequest) {
        // FIXME: check pegin proof and mark as used (ATOMICITY!!!)
        let issuance_req = peg_in.blind_tokens;
        debug!("Signing issuance request {}", issuance_req.id());
        let signed_req = self.mint.sign(issuance_req);

        self.db
            .transaction(|tree| {
                tree.insert_entry(
                    &ConsensusItem::PartiallySignedRequest(signed_req.clone()),
                    &(),
                )?;
                tree.insert_entry(
                    &PartialSignatureKey {
                        request_id: signed_req.id(),
                        peer_id: self.cfg.identity,
                    },
                    &signed_req,
                )?;
                Ok(())
            })
            .expect("DB error");
    }

    fn process_reissuance_request(&mut self, peer: u16, reissuance: ReissuanceRequest) {
        let signed_request = match self.mint.reissue(reissuance.coins, reissuance.blind_tokens) {
            Some(sr) => sr,
            None => {
                warn!("Rejected reissuance request proposed by peer {}", peer);
                return;
            }
        };
        debug!("Signed reissuance request {}", signed_request.id());

        self.db
            .transaction(|tree| {
                tree.insert_entry(
                    &ConsensusItem::PartiallySignedRequest(signed_request.clone()),
                    &(),
                )?;
                tree.insert_entry(
                    &PartialSignatureKey {
                        request_id: signed_request.id(),
                        peer_id: self.cfg.identity,
                    },
                    &signed_request,
                )?;

                Ok(())
            })
            .expect("DB error");
    }

    fn process_partial_signature(
        &mut self,
        peer: u16,
        partial_sig: PartialSigResponse,
    ) -> Option<SigResponse> {
        let req_id = partial_sig.id();
        let tbs_thresh = self.tbs_threshold();
        debug!(
            "Received sig share from peer {} for issuance {}",
            peer, req_id
        );

        let existed = self
            .db
            .insert_entry(
                &PartialSignatureKey {
                    request_id: req_id,
                    peer_id: peer,
                },
                &partial_sig,
            )
            .expect("DB error");

        if let Some(ex) = existed {
            warn!("Peer {} submitted signature share twice", peer);
            if ex != partial_sig {
                error!("Peer {} submitted two different signature shares", peer);
            }
        }

        let req_psigs = self
            .db
            .find_by_prefix::<_, PartialSignatureKey, _>(&PartialSignaturesPrefixKey {
                request_id: req_id,
            })
            .map(|entry_res| entry_res.map(|(key, value)| (key.peer_id as usize, value)))
            .collect::<Result<Vec<_>, _>>()
            .expect("DB error");

        if req_psigs.len() > tbs_thresh {
            debug!(
                "Trying to combine sig shares for issuance request {}",
                req_id
            );
            let (bsig, errors) = self.mint.combine(req_psigs);
            if !errors.0.is_empty() {
                warn!("Peer sent faulty share: {:?}", errors);
            }

            match bsig {
                Ok(bsig) => {
                    debug!(
                        "Successfully combined signature shares for issuance request {}",
                        req_id
                    );

                    let removal_keys = self
                        .db
                        .find_by_prefix(&PartialSignaturesPrefixKey { request_id: req_id })
                        .map(|entry_res| {
                            entry_res.map(|(key, _): (PartialSignatureKey, PartialSigResponse)| key)
                        })
                        .collect::<Result<Vec<PartialSignatureKey>, _>>()
                        .expect("DB error");
                    self.db
                        .transaction(|tree| {
                            for key in removal_keys.iter() {
                                tree.remove_entry::<_, PartialSigResponse>(key)?;
                            }
                            Ok(())
                        })
                        .expect("DB error");

                    return Some(bsig);
                }
                Err(e) => {
                    error!("Warn: could not combine shares: {:?}", e);
                }
            }
        }

        None
    }

    fn tbs_threshold(&self) -> usize {
        self.cfg.peers.len() - self.cfg.max_faulty() - 1
    }
}

const DB_PREFIX_CONSENSUS_ITEM: u8 = 1;

impl DatabaseEncode for ConsensusItem {
    fn to_bytes(&self) -> IVec {
        let mut bytes = vec![DB_PREFIX_CONSENSUS_ITEM];
        bincode::serialize_into(&mut bytes, &self).unwrap(); // TODO: use own encoding
        bytes.into()
    }
}

impl DatabaseDecode for ConsensusItem {
    fn from_bytes(data: &IVec) -> Result<Self, DecodingError> {
        // TODO: Distinguish key and value encoding
        if let Some(&typ) = data.first() {
            if typ != DB_PREFIX_CONSENSUS_ITEM {
                return Err(DecodingError("Wrong type".into()));
            }
        } else {
            return Err(DecodingError("No type field".into()));
        }

        bincode::deserialize(&data[1..]).map_err(|e| DecodingError(e.into()))
    }
}

struct ConsensusItemKeyPrefix;

impl DatabaseEncode for ConsensusItemKeyPrefix {
    fn to_bytes(&self) -> IVec {
        (&[DB_PREFIX_CONSENSUS_ITEM][..]).into()
    }
}

const DB_PREFIX_PARTIAL_SIG: u8 = 2;

struct PartialSignatureKey {
    request_id: u64,
    peer_id: u16,
}

impl DatabaseEncode for PartialSignatureKey {
    fn to_bytes(&self) -> IVec {
        let mut bytes = Vec::with_capacity(11);
        bytes.push(DB_PREFIX_PARTIAL_SIG);
        bytes.extend_from_slice(&self.request_id.to_be_bytes()[..]);
        bytes.extend_from_slice(&self.peer_id.to_be_bytes()[..]);
        bytes.into()
    }
}

impl DatabaseDecode for PartialSignatureKey {
    fn from_bytes(data: &IVec) -> Result<Self, DecodingError> {
        if data.len() != 11 {
            return Err(DecodingError(
                "Expected 11 bytes, got something else".into(),
            ));
        }

        if data[0] != DB_PREFIX_PARTIAL_SIG {
            return Err(DecodingError(
                "Expected partial sig, got something else".into(),
            ));
        }

        let mut request_id_bytes = [0u8; 8];
        request_id_bytes.copy_from_slice(&data[1..9]);
        let request_id = u64::from_be_bytes(request_id_bytes);

        let mut peer_id_bytes = [0u8; 2];
        peer_id_bytes.copy_from_slice(&data[9..11]);
        let peer_id = u16::from_be_bytes(peer_id_bytes);

        Ok(PartialSignatureKey {
            request_id,
            peer_id,
        })
    }
}

impl DatabaseEncode for PartialSigResponse {
    fn to_bytes(&self) -> IVec {
        bincode::serialize(&self)
            .expect("Serialization error")
            .into()
    }
}

impl DatabaseDecode for PartialSigResponse {
    fn from_bytes(data: &IVec) -> Result<Self, DecodingError> {
        bincode::deserialize(&data).map_err(|e| DecodingError(e.into()))
    }
}

struct PartialSignaturesPrefixKey {
    request_id: u64,
}

impl DatabaseEncode for PartialSignaturesPrefixKey {
    fn to_bytes(&self) -> IVec {
        let mut bytes = Vec::with_capacity(9);
        bytes.push(DB_PREFIX_PARTIAL_SIG);
        bytes.extend_from_slice(&self.request_id.to_be_bytes()[..]);
        bytes.into()
    }
}

#[derive(Debug, Error)]
pub enum ClientRequestError {
    #[error("Client Reuqest was not authorized with a valid signature")]
    InvalidTransactionSignature,
    #[error("Client request was denied by mint (double spend or invalid mint signature)")]
    DeniedByMint,
}
