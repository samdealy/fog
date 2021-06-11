// Copyright (c) 2018-2021 The MobileCoin Foundation
//! Object representing trusted storage for key image records.
//! Mediates between the bytes used in ORAM and the protobuf format,
//! the various ORAM vs. fog api error codes, etc.
#![allow(unused)]
use aligned_cmov::{
    subtle::{Choice, ConstantTimeEq},
    typenum::{Unsigned, U1024, U16, U32, U4096, U64},
    A8Bytes, CMov,
};
use alloc::{boxed::Box, string::String, vec::Vec};
use core::convert::TryInto;
use fog_ledger_enclave_api::{AddRecordsError, Error, Error::AddRecords};
use fog_types::ledger::KeyImageResultCode;
use mc_common::logger::Logger;
use mc_crypto_rand::McRng;
use mc_oblivious_map::CuckooHashTableCreator;
use mc_oblivious_ram::PathORAM4096Z4Creator;
use mc_oblivious_traits::{
    OMapCreator, ORAMStorageCreator, ObliviousHashMap, OMAP_FOUND, OMAP_INVALID_KEY,
    OMAP_NOT_FOUND, OMAP_OVERFLOW,
};
use mc_transaction_core::{ring_signature::KeyImage, BlockIndex};
use fog_ledger_enclave_api::messages::KeyImageData;

// internal constants
// KeySize and ValueSize reflect the needs of key_image_store
// We must choose an oblivious map algorithm that can support that
type KeySize = U32;
type ValueSize = U16;
// BlockSize is a tuning parameter for OMap which must become the ValueSize of
// the selected ORAM
type BlockSize = U1024;
// This selects an oblivious ram algorithm which can support queries of size
// BlockSize The ORAMStorageCreator type is a generic parameter to KeyImageStore
type ObliviousRAMAlgo<OSC> = PathORAM4096Z4Creator<McRng, OSC>;
// These are the requirements on the storage, this is imposed by the choice of
// oram algorithm
pub type StorageDataSize = U4096;
pub type StorageMetaSize = U64;

// This selects the stash size we will construct the oram with
const STASH_SIZE: usize = 32;
// This selects the oblivious map algorithm
type ObliviousMapCreator<OSC> = CuckooHashTableCreator<BlockSize, McRng, ObliviousRAMAlgo<OSC>>;

/// Object which holds ORAM and services KeyImageRecord requests
///
/// This object handles translations between protobuf types, and the aligned
/// chunks of bytes Key and Value used in the oblivious map interface.
///
/// - The size in the OMAP is ValueSize which must be divisible by 8,
/// - The user actually gives us a serialized protobuf
/// - We use a wire format in the omap where value[0] = ValueSize - 1 -
///   ciphertext.len(), ValueSize must be within 255 bytes of ciphertext.len().
/// - When the lookup misses, we try to obliviously return a buffer of the
///   normal size. We do this by remembering the ciphertext size byte of the
///   last stored ciphertext.
pub struct KeyImageStore<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> {
    /// Oblivious map to hold KeyImageStoreRecords
    omap: Box<<ObliviousMapCreator<OSC> as OMapCreator<KeySize, ValueSize, McRng>>::Output>,

    /// The logger object
    logger: Logger,
}

impl<OSC: ORAMStorageCreator<StorageDataSize, StorageMetaSize>> KeyImageStore<OSC> {
    pub fn new(desired_capacity: u64, logger: Logger) -> Self {
        Self {
            omap: Box::new(<ObliviousMapCreator<OSC> as OMapCreator<
                KeySize,
                ValueSize,
                McRng,
            >>::create(
                desired_capacity, STASH_SIZE, McRng::default
            )),
            logger,
        }
    }

    // add a key image containing block index and timestamp
    pub fn add_record(&mut self, key_image: &KeyImage, data: KeyImageData) -> Result<(), AddRecordsError> {
        let mut value = A8Bytes::<ValueSize>::default();
        let mut key = A8Bytes::<KeySize>::default(); // key used to add to the oram for key image
        key.clone_from_slice(&key_image.as_ref());
        // write block index data to  value[0..8] write the time stamp data to
        // value[8..16]
        value[0..8].clone_from_slice(&data.block_index.to_le_bytes());
        value[8..16].clone_from_slice(&data.timestamp.to_le_bytes());
        // Note: Passing true means we allow overwrite, which seems fine since
        // the value is not changing
        let omap_result_code = self.omap.vartime_write(&key, &value, Choice::from(1));
        if omap_result_code == OMAP_INVALID_KEY {
            return Err(AddRecordsError::KeyWrongSize);
        } else if omap_result_code == OMAP_OVERFLOW {
            return Err(AddRecordsError::KeyWrongSize);
        } else if omap_result_code == OMAP_FOUND {
            // log::debug!(
            //    self.logger,
            //    "An omap key was added twice, overwriting previous value"
            // );
        } else if omap_result_code != OMAP_NOT_FOUND {
            panic!(
                "omap_result_code had an unexpected value: {}",
                omap_result_code
            );
        }
        Ok(())
    }

    // return new struct KeyImageData which contains block index and timestamp of
    // key image key image as ref to convert key image to 32 bits,
    // call the oram to query to to key image data
    pub fn find_record(&mut self, key_image: &KeyImage) -> (KeyImageData, KeyImageResultCode) {
        // find_record is reusing KeyImageResultCode
        let mut result = KeyImageData {
            block_index: 0u64,
            timestamp: 0u64,
        };

        let mut result_code = KeyImageResultCode::KeyImageError as u32;
        let mut key = A8Bytes::<KeySize>::default(); // key used to query the oram for key image
        key.clone_from_slice(&key_image.as_ref());

        let mut value = A8Bytes::<ValueSize>::default(); // value used to save the reuslt of querying
                                                         //the oram for key image value using key

        // Do ORAM read operation and branchlessly handle the result code
        // OMAP_FOUND -> KeyImageResultCode::Unused
        // OMAP_NOT_FOUND -> KeyImageResultCode::KeyImageError
        // OMAP_INVALID_KEY -> KeyImageResultCode::KeyImageError
        // Other -> KeyImageResultCode::KeyImageError debug_assert!(false)
        {
            let oram_result_code = self.omap.read(&key, &mut value);
            result_code.cmov(
                oram_result_code.ct_eq(&OMAP_FOUND),
                &(KeyImageResultCode::NotSpent as u32),
            );
            result_code.cmov(
                oram_result_code.ct_eq(&OMAP_NOT_FOUND),
                &(KeyImageResultCode::KeyImageError as u32),
            );
            result_code.cmov(
                oram_result_code.ct_eq(&OMAP_INVALID_KEY),
                &(KeyImageResultCode::KeyImageError as u32),
            );
            // This is debug assert to avoid creating a branch in production
            debug_assert!(
                oram_result_code == OMAP_FOUND
                    || oram_result_code == OMAP_NOT_FOUND
                    || oram_result_code == OMAP_INVALID_KEY,
                "oram_result_code had an unexpected value: {}",
                oram_result_code
            );
        }

        // Copy the data in value[0..8] to result.block_index
        // Copy the data in value[8..16] to result.timestamp
        result.block_index = u64::from_le_bytes(value[0..8].try_into().unwrap());
        result.timestamp = u64::from_le_bytes(value[8..16].try_into().unwrap());

        if (result_code == OMAP_FOUND) {
            (result, KeyImageResultCode::NotSpent)
        } else if (result_code == OMAP_NOT_FOUND) {
            (result, KeyImageResultCode::KeyImageError)
        } else {
            (result, KeyImageResultCode::KeyImageError)
        }
    }
}