// Bitcoin Dev Kit
// Written in 2020 by Alekos Filini <alekos.filini@gmail.com>
//
// Copyright (c) 2020-2021 Bitcoin Dev Kit Developers
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Proof of reserves
//!
//! This module provides the ability to create proofs of reserves.
//! A proof is a valid but unspendable transaction. By signing a transaction
//! that spends some UTXOs we are proofing that we have control over these funds.
//! The implementation is inspired by the following BIPs:
//! https://github.com/bitcoin/bips/blob/master/bip-0127.mediawiki
//! https://github.com/bitcoin/bips/blob/master/bip-0322.mediawiki

use bdk::bitcoin::blockdata::opcodes;
use bdk::bitcoin::blockdata::script::{Builder, Script};
use bdk::bitcoin::blockdata::transaction::{EcdsaSighashType, OutPoint, TxIn, TxOut};
use bdk::bitcoin::consensus::encode::serialize;
use bdk::bitcoin::hash_types::{PubkeyHash, Txid};
use bdk::bitcoin::hashes::{hash160, sha256d, Hash};
use bdk::bitcoin::util::psbt::{Input, raw::Key, PartiallySignedTransaction as PSBT};
use bdk::bitcoin::{Sequence, Transaction};
use bdk::database::BatchDatabase;
use bdk::wallet::tx_builder::TxOrdering;
use bdk::wallet::Wallet;
use bdk::Error;

use std::collections::BTreeMap;

pub const PSBT_IN_POR_COMMITMENT: u8 = 0x09;

/// The API for proof of reserves
pub trait ProofOfReserves {
    /// Create a proof for all spendable UTXOs in a wallet
    fn create_proof(&self, message: &str) -> Result<PSBT, ProofError>;

    /// Make sure this is a proof, and not a spendable transaction.
    /// Make sure the proof is valid.
    /// Currently proofs can only be validated against the tip of the chain.
    /// If some of the UTXOs in the proof were spent in the meantime, the proof will fail.
    /// We can currently not validate whether it was valid at a certain block height.
    /// With the max_block_height parameter the caller can ensure that only UTXOs with sufficient confirmations are considered.
    /// If no max_block_height is provided, also UTXOs from transactions in the mempool are considered.
    /// Returns the spendable amount of the proof.
    fn verify_proof(
        &self,
        psbt: &PSBT,
        message: &str,
        max_block_height: Option<u32>,
    ) -> Result<u64, ProofError>;
}

/// Proof error
#[derive(Debug)]
pub enum ProofError {
    /// Less than two inputs
    WrongNumberOfInputs,
    /// Must have exactly 1 output
    WrongNumberOfOutputs,
    /// Challenge input does not match
    ChallengeInputMismatch,
    /// Found an input other than the challenge which is not spendable. Holds the position of the input.
    NonSpendableInput(usize),
    /// Found an input that has no signature at position
    NotSignedInput(usize),
    /// Found an input with an unsupported SIGHASH type at position
    UnsupportedSighashType(usize),
    /// Found an input that is neither witness nor legacy at position
    NeitherWitnessNorLegacy(usize),
    /// Signature validation failed
    SignatureValidation(usize, String),
    /// The output is not valid
    InvalidOutput,
    /// Input and output values are not equal, implying a miner fee
    InAndOutValueNotEqual,
    /// No matching outpoint found
    OutpointNotFound(usize),
    /// Failed to retrieve the block height of a Tx or UTXO
    MissingConfirmationInfo,
    /// A wrapped BDK Error
    BdkError(Error),
}

impl From<bdk::Error> for ProofError {
    fn from(error: bdk::Error) -> Self {
        ProofError::BdkError(error)
    }
}

impl From<ProofError> for bdk::Error {
    fn from(error: ProofError) -> Self {
        if let ProofError::BdkError(err) = error {
            err
        } else {
            bdk::Error::Generic(format!("{:?}", error))
        }
    }
}

impl<D> ProofOfReserves for Wallet<D>
where
    D: BatchDatabase,
{
    fn create_proof(&self, message: &str) -> Result<PSBT, ProofError> {
        if message.is_empty() {
            return Err(ProofError::ChallengeInputMismatch);
        }
        let challenge_txin = challenge_txin(message);

        let challenge_key = Key {
            type_value: PSBT_IN_POR_COMMITMENT,
            key: Vec::new(),
        };

        let mut unknown_psbt_keys: BTreeMap<Key, Vec<u8>> = BTreeMap::new();
        unknown_psbt_keys.insert(challenge_key, message.as_bytes().into());

        let challenge_psbt_inp = Input {
            witness_utxo: Some(TxOut {
                value: 0,
                script_pubkey: Builder::new().push_opcode(opcodes::OP_TRUE).into_script(),
            }),
            final_script_sig: Some(Script::default()), /* "finalize" the input with an empty scriptSig */
            unknown: unknown_psbt_keys,
            ..Default::default()
        };

        let pkh = PubkeyHash::from_hash(hash160::Hash::hash(&[0]));
        let out_script_unspendable = Script::new_p2pkh(&pkh);

        let mut builder = self.build_tx();
        builder
            .drain_wallet()
            .add_foreign_utxo(challenge_txin.previous_output, challenge_psbt_inp, 42)?
            .fee_absolute(0)
            .only_witness_utxo()
            .current_height(0)
            .drain_to(out_script_unspendable)
            .ordering(TxOrdering::Untouched);
        let (psbt, _details) = builder.finish().map_err(ProofError::BdkError)?;

        Ok(psbt)
    }

    fn verify_proof(
        &self,
        psbt: &PSBT,
        message: &str,
        max_block_height: Option<u32>,
    ) -> Result<u64, ProofError> {
        // verify the proof UTXOs are still spendable
        let unspents = match self.list_unspent() {
            Ok(utxos) => utxos,
            Err(err) => return Err(ProofError::BdkError(err)),
        };
        let unspents = unspents
            .iter()
            .map(|utxo| {
                if max_block_height.is_none() {
                    Ok((utxo, None))
                } else {
                    let tx_details = self.get_tx(&utxo.outpoint.txid, false)?;
                    if let Some(tx_details) = tx_details {
                        if let Some(conf_time) = tx_details.confirmation_time {
                            Ok((utxo, Some(conf_time.height)))
                        } else {
                            Ok((utxo, None))
                        }
                    } else {
                        Err(ProofError::MissingConfirmationInfo)
                    }
                }
            })
            .collect::<Result<Vec<_>, ProofError>>()?;
        let outpoints = unspents
            .iter()
            .filter(|(_utxo, block_height)| {
                block_height.unwrap_or(u32::MAX) <= max_block_height.unwrap_or(u32::MAX)
            })
            .map(|(utxo, _)| (utxo.outpoint, utxo.txout.clone()))
            .collect();

        verify_proof(psbt, message, outpoints)
    }
}

/// Trait for Transaction-centric proofs
trait ReserveProof {
    /// Verify a proof transaction.
    /// Look up utxos with f()
    fn verify_reserve<F>(&self, message: &str, f: F) -> Result<u64, ProofError>
    where
        F: FnMut(&OutPoint) -> Option<TxOut>;
}

impl ReserveProof for Transaction {
    fn verify_reserve<F>(&self, message: &str, mut f: F) -> Result<u64, ProofError>
    where
        F: FnMut(&OutPoint) -> Option<TxOut>
    {
        if self.output.len() != 1 {
            return Err(ProofError::WrongNumberOfOutputs);
        }
        if self.input.len() <= 1 {
            return Err(ProofError::WrongNumberOfInputs);
        }

        // verify the unspendable output
        let pkh = PubkeyHash::from_hash(hash160::Hash::hash(&[0]));
        let out_script_unspendable = Script::new_p2pkh(&pkh);

        if self.output[0].script_pubkey != out_script_unspendable {
            return Err(ProofError::InvalidOutput);
        }

        // verify the challenge txin
        let challenge_input = challenge_txin(message);
        if self.input[0].previous_output != challenge_input.previous_output {
            return Err(ProofError::ChallengeInputMismatch);
        }

        // gather the proof UTXOs
        let prevouts: Vec<(usize, TxOut)> = self
            .input
            .iter()
            .enumerate()
            .skip(1)
            .map(|(i, input)|
                f(&input.previous_output)
                    .map(|txout| (i, txout))
                    .ok_or(ProofError::NonSpendableInput(i))
            )
            .collect::<Result<_, ProofError>>()?;

        let sum: u64 = prevouts.iter()
            .map(|(_i, prevout)| prevout.value)
            .sum();

        // inflow and outflow being equal means no miner fee
        if self.output[0].value != sum {
            return Err(ProofError::InAndOutValueNotEqual);
        }

        let serialized_tx = serialize(&self);

        // Check that all inputs besides the challenge input are valid
        prevouts
            .iter()
            .map(|(i, prevout)|
                bitcoinconsensus::verify(
                    prevout.script_pubkey.to_bytes().as_slice(),
                    prevout.value,
                    &serialized_tx,
                    *i,
                )
                .map_err(|e|
                    ProofError::SignatureValidation(*i, format!("{:?}", e))
                ),
            )
            .collect::<Result<(), _>>()?;

        // Check that all other inputs actually commit to challenge
        // input by modifying the challenge input and verifying that
        // validation *fails*.
        //
        // If validation succeeds here, that input did not correctly
        // commit to the challenge input.
        let serialized_malleated_tx = {
            let mut malleated_tx = self.clone();

            let mut malleated_message = "MALLEATED: ".to_string();
            malleated_message.push_str(message);

            malleated_tx.input[0] = challenge_txin(&malleated_message);

            serialize(&malleated_tx)
        };

        prevouts
            .iter()
            .map(|(i, prevout)|
                match bitcoinconsensus::verify(
                    prevout.script_pubkey.to_bytes().as_slice(),
                    prevout.value,
                    &serialized_malleated_tx,
                    *i,
                ) {
                    Ok(_) => {
                        Err(ProofError::SignatureValidation(*i, "Does not commit to challenge input".to_string()))
                    },
                    Err(_) => {
                        Ok(())
                    }
                }
            )
            .collect::<Result<(), _>>()?;

        Ok(sum)
    }
}

/// Make sure this is a proof, and not a spendable transaction.
/// Make sure the proof is valid.
/// Currently proofs can only be validated against the tip of the chain.
/// If some of the UTXOs in the proof were spent in the meantime, the proof will fail.
/// We can currently not validate whether it was valid at a certain block height.
/// Since the caller provides the outpoints, he is also responsible to make sure they have enough confirmations.
/// Returns the spendable amount of the proof.
pub fn verify_proof(
    psbt: &PSBT,
    message: &str,
    outpoints: Vec<(OutPoint, TxOut)>,
) -> Result<u64, ProofError> {
    let tx = psbt.clone().extract_tx();

    // Redundant check to tx.verify_reserve() to ensure error priority is not changed
    if tx.output.len() != 1 {
        return Err(ProofError::WrongNumberOfOutputs);
    }

    // verify that the inputs are signed, except the challenge
    if let Some((i, _inp)) = psbt
        .inputs
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_i, inp)| inp.final_script_sig.is_none() && inp.final_script_witness.is_none())
    {
        return Err(ProofError::NotSignedInput(i));
    }

    // Verify the SIGHASH
    if let Some((i, _psbt_in)) = psbt.inputs.iter().enumerate().find(|(_i, psbt_in)| {
        psbt_in.sighash_type.is_some() && psbt_in.sighash_type != Some(EcdsaSighashType::All.into())
    }) {
        return Err(ProofError::UnsupportedSighashType(i));
    }

    let serialized_tx = serialize(&tx);

    let challenge_input = psbt.inputs.get(0)
        .ok_or(ProofError::WrongNumberOfInputs)?;

    // Verify the challenge input
    if let Some(utxo) = &challenge_input.witness_utxo {
        if let Err(err) = bitcoinconsensus::verify(
            utxo.script_pubkey.to_bytes().as_slice(),
            utxo.value,
            &serialized_tx,
            0,
        ) {
            return Err(ProofError::SignatureValidation(0, format!("{:?}", err)));
        }
    } else {
        return Err(ProofError::SignatureValidation(
            0,
            "witness_utxo not found for challenge input".to_string(),
        ));
    }

    tx.verify_reserve(message, |search_outpoint|
        outpoints
            .iter()
            .find_map(|(outpoint, txout)|
                if search_outpoint == outpoint {
                    Some(txout.clone())
                } else {
                    None
                }
            )
    )
}

/// Construct a challenge input with the message
fn challenge_txin(message: &str) -> TxIn {
    let message = "Proof-of-Reserves: ".to_string() + message;
    let message = sha256d::Hash::hash(message.as_bytes());
    TxIn {
        previous_output: OutPoint::new(Txid::from_hash(message), 0),
        sequence: Sequence(0xFFFFFFFF),
        ..Default::default()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use base64ct::{Base64, Encoding};
    use bdk::bitcoin::consensus::encode::deserialize;
    use bdk::wallet::get_funded_wallet;

    #[test]
    fn test_proof() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let psbt = wallet.create_proof(message).unwrap();
        let psbt_ser = serialize(&psbt);
        let psbt_b64 = Base64::encode_string(&psbt_ser);

        let expected = r#"cHNidP8BAH4BAAAAAmw1RvG4UzfnSafpx62EPTyha6VslP0Er7n3TxjEpeBeAAAAAAD/////2johM0znoXIXT1lg+ySrvGrtq1IGXPJzpfi/emkV9iIAAAAAAP////8BUMMAAAAAAAAZdqkUn3/QltN+0sDj9/DPySS+70/862iIrAAAAAAAAQEKAAAAAAAAAAABUQEHAAEJE1RoaXMgYmVsb25ncyB0byBtZS4AAQEfUMMAAAAAAAAWABTs5SZXEFPahkVMngZneer50VAuaiIGAysFWAeL7DhpSoSTPWWTA+JXXa5+kWhZEUVBFb/WRIfjBOzlJlcAAA=="#;

        assert_eq!(psbt_b64, expected);
    }

    #[test]
    #[should_panic(
        expected = "Descriptor(Miniscript(Unexpected(\"unexpected «Key too short (<66 char), doesn't match any format»\")))"
    )]
    fn invalid_descriptor() {
        let descriptor = "wpkh(cVpPVqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let _psbt = wallet.create_proof(message).unwrap();
    }

    #[test]
    #[should_panic(expected = "ChallengeInputMismatch")]
    fn empty_message() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "";
        let _psbt = wallet.create_proof(message).unwrap();
    }

    fn get_signed_proof() -> PSBT {
        let psbt = "cHNidP8BAH4BAAAAAmw1RvG4UzfnSafpx62EPTyha6VslP0Er7n3TxjEpeBeAAAAAAD/////2johM0znoXIXT1lg+ySrvGrtq1IGXPJzpfi/emkV9iIAAAAAAP////8BUMMAAAAAAAAZdqkUn3/QltN+0sDj9/DPySS+70/862iIrAAAAAAAAQEKAAAAAAAAAAABUQEHAAABAR9QwwAAAAAAABYAFOzlJlcQU9qGRUyeBmd56vnRUC5qAQcAAQhrAkcwRAIgDSE4PQ57JDiZ7otGkTqz35bi/e1pexYaYKWaveuvRd4CIFzVB4sAmgtdEVz2vHzs1iXc9iRKJ+KQOQb+C2DtPyvzASEDKwVYB4vsOGlKhJM9ZZMD4lddrn6RaFkRRUEVv9ZEh+MAAA==";
        let psbt = Base64::decode_vec(psbt).unwrap();
        deserialize(&psbt).unwrap()
    }

    #[test]
    fn verify_internal() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let psbt = get_signed_proof();
        let spendable = wallet.verify_proof(&psbt, message, None).unwrap();
        assert_eq!(spendable, 50_000);
    }

    #[test]
    #[should_panic(expected = "NonSpendableInput")]
    fn verify_internal_90() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let psbt = get_signed_proof();
        let spendable = wallet.verify_proof(&psbt, message, Some(90)).unwrap();
        assert_eq!(spendable, 50_000);
    }

    #[test]
    fn verify_internal_100() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let psbt = get_signed_proof();
        let spendable = wallet.verify_proof(&psbt, message, Some(100)).unwrap();
        assert_eq!(spendable, 50_000);
    }

    #[test]
    fn verify_external() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let psbt = get_signed_proof();
        let unspents = wallet.list_unspent().unwrap();
        let outpoints = unspents
            .iter()
            .map(|utxo| (utxo.outpoint, utxo.txout.clone()))
            .collect();
        let spendable = verify_proof(&psbt, message, outpoints).unwrap();

        assert_eq!(spendable, 50_000);
    }

    #[test]
    #[should_panic(expected = "ChallengeInputMismatch")]
    fn wrong_message() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "Wrong message!";
        let psbt = get_signed_proof();
        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "WrongNumberOfInputs")]
    fn too_few_inputs() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();
        psbt.unsigned_tx.input.truncate(1);
        psbt.inputs.truncate(1);

        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "WrongNumberOfOutputs")]
    fn no_output() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();
        psbt.unsigned_tx.output.clear();
        psbt.inputs.clear();

        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "NotSignedInput")]
    fn missing_signature() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();
        psbt.inputs[1].final_script_sig = None;
        psbt.inputs[1].final_script_witness = None;

        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "UnsupportedSighashType(1)")]
    fn wrong_sighash_type() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();
        psbt.inputs[1].sighash_type = Some(EcdsaSighashType::SinglePlusAnyoneCanPay.into());

        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "InvalidOutput")]
    fn invalid_output() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();

        let pkh = PubkeyHash::from_hash(hash160::Hash::hash(&[0, 1, 2, 3]));
        let out_script_unspendable = Script::new_p2pkh(&pkh);
        psbt.unsigned_tx.output[0].script_pubkey = out_script_unspendable;

        wallet.verify_proof(&psbt, message, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "InAndOutValueNotEqual")]
    fn sum_mismatch() {
        let descriptor = "wpkh(cVpPVruEDdmutPzisEsYvtST1usBR3ntr8pXSyt6D2YYqXRyPcFW)";
        let (wallet, _, _) = get_funded_wallet(descriptor);

        let message = "This belongs to me.";
        let mut psbt = get_signed_proof();
        psbt.unsigned_tx.output[0].value = 123;

        wallet.verify_proof(&psbt, message, None).unwrap();
    }
}
