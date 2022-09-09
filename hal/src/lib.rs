//! Implementation of a HAL service for KeyMint.
//!
//! This implementation relies on a `SerializedChannel` abstraction for a communication channel to
//! the trusted application (TA).  Incoming method invocations for the HAL service are converted
//! into corresponding request structures, which are then serialized (using CBOR) and send down the
//! channel.  A serialized response is then read from the channel, which is deserialized into a
//! response structure.  The contents of this response structure are then used to populate the
//! return values of the HAL service method.

#![allow(non_snake_case)]

use core::{convert::TryInto, fmt::Debug};
use kmr_common::{
    cbor,
    wire::{keymint::ErrorCode, Code, KeyMintOperation},
    AsCborValue, CborError,
};
use log::{error, info};
use std::{
    ffi::CString,
    io::{Read, Write},
    ops::DerefMut,
    sync::MutexGuard,
};

pub use binder;

pub mod env;
pub mod hal;
pub mod keymint;
pub mod rpc;
pub mod secureclock;
pub mod sharedsecret;
#[cfg(test)]
mod tests;

/// Emit a failure for a failed CBOR conversion.
#[inline]
pub fn failed_cbor(err: CborError) -> binder::Status {
    binder::Status::new_service_specific_error(
        ErrorCode::UnknownError as i32,
        Some(&CString::new(format!("CBOR conversion failed: {:?}", err)).unwrap()),
    )
}

/// Abstraction of a channel to a secure world TA implementation, which accepts serialized request
/// messages and returns serialized return values (or an error if communication via the channel is
/// lost).
pub trait SerializedChannel: Debug + Send {
    fn execute(&mut self, serialized_req: &[u8]) -> binder::Result<Vec<u8>>;
}

/// Write a message to a stream-oriented [`Write`] item, with length framing.
pub fn write_msg<W: Write>(w: &mut W, data: &[u8]) -> binder::Result<()> {
    // The underlying `Write` item does not guarantee delivery of complete messages.
    // Make this possible by adding framing in the form of a big-endian `u32` holding
    // the message length.
    let data_len: u32 = data.len().try_into().map_err(|_e| {
        binder::Status::new_exception(
            binder::ExceptionCode::BAD_PARCELABLE,
            Some(&CString::new("encoded request message too large").unwrap()),
        )
    })?;
    let data_len_data = data_len.to_be_bytes();
    w.write_all(&data_len_data[..]).map_err(|e| {
        error!("Failed to write length to stream: {}", e);
        binder::Status::new_exception(
            binder::ExceptionCode::BAD_PARCELABLE,
            Some(&CString::new("failed to write framing length").unwrap()),
        )
    })?;
    w.write_all(data).map_err(|e| {
        error!("Failed to write data to stream: {}", e);
        binder::Status::new_exception(
            binder::ExceptionCode::BAD_PARCELABLE,
            Some(&CString::new("failed to write data").unwrap()),
        )
    })?;
    Ok(())
}

/// Read a message from a stream-oriented [`Read`] item, with length framing.
pub fn read_msg<R: Read>(r: &mut R) -> binder::Result<Vec<u8>> {
    // The data read from the `Read` item has a 4-byte big-endian length prefix.
    let mut len_data = [0u8; 4];
    r.read_exact(&mut len_data).map_err(|e| {
        error!("Failed to read length from stream: {}", e);
        binder::Status::new_exception(binder::ExceptionCode::TRANSACTION_FAILED, None)
    })?;
    let len = u32::from_be_bytes(len_data);
    let mut data = vec![0; len as usize];
    r.read_exact(&mut data).map_err(|e| {
        error!("Failed to read data from stream: {}", e);
        binder::Status::new_exception(binder::ExceptionCode::TRANSACTION_FAILED, None)
    })?;
    Ok(data)
}

/// Message-oriented wrapper around a pair of stream-oriented channels.  This allows a pair of
/// uni-directional channels that don't necessarily preserve message boundaries to appear as a
/// single bi-directional channel that does preserve message boundaries.
#[derive(Debug)]
pub struct MessageChannel<R: Read, W: Write> {
    r: R,
    w: W,
}

impl<R: Read + Debug + Send, W: Write + Debug + Send> SerializedChannel for MessageChannel<R, W> {
    fn execute(&mut self, serialized_req: &[u8]) -> binder::Result<Vec<u8>> {
        write_msg(&mut self.w, serialized_req)?;
        read_msg(&mut self.r)
    }
}

/// Execute an operation by serializing and sending a request structure down a channel, and
/// deserializing and returning the response.
///
/// This implementation relies on the internal serialization format for `PerformOpReq` and
/// `PerformOpRsp` to allow direct use of the specific request/response types.
fn channel_execute<T, R, S>(channel: &mut T, req: R) -> binder::Result<S>
where
    T: SerializedChannel,
    R: AsCborValue + Code<KeyMintOperation>,
    S: AsCborValue + Code<KeyMintOperation>,
{
    // Manually build an array that includes the opcode and the encoded request and encode it.
    // This is equivalent to `PerformOpReq::to_vec()`.
    let req_arr = cbor::value::Value::Array(vec![
        <R>::CODE.to_cbor_value().map_err(failed_cbor)?,
        req.to_cbor_value().map_err(failed_cbor)?,
    ]);
    let mut req_data = Vec::new();
    cbor::ser::into_writer(&req_arr, &mut req_data).map_err(|e| {
        binder::Status::new_service_specific_error(
            ErrorCode::UnknownError as i32,
            Some(
                &CString::new(format!("failed to write CBOR request to buffer: {:?}", e)).unwrap(),
            ),
        )
    })?;

    if req_data.len() > kmr_common::wire::MAX_SIZE {
        error!(
            "HAL operation {:?} encodes bigger {} than max size {}",
            <R>::CODE,
            req_data.len(),
            kmr_common::wire::MAX_SIZE
        );
        return Err(binder::Status::new_service_specific_error(
            ErrorCode::InvalidInputLength as i32,
            Some(&CString::new("encoded request message too large").unwrap()),
        ));
    }

    // Send in request bytes, get back response bytes.
    let rsp_data = channel.execute(&req_data)?;

    // Convert the raw response data to an array of [error code, opt_response].
    let rsp_value = kmr_common::read_to_value(&rsp_data).map_err(failed_cbor)?;
    let mut rsp_array = match rsp_value {
        cbor::value::Value::Array(a) if a.len() == 2 => a,
        _ => {
            error!("HAL: failed to parse response data 2-array!");
            return kmr_common::cbor_type_error(&rsp_value, "arr of len 2").map_err(failed_cbor);
        }
    };
    let opt_response = rsp_array.remove(1);
    let error_code = <ErrorCode>::from_cbor_value(rsp_array.remove(0)).map_err(failed_cbor)?;
    if error_code != ErrorCode::Ok {
        error!("HAL: command {:?} failed: {:?}", <R>::CODE, error_code);
        return Err(binder::Status::new_service_specific_error(error_code as i32, None));
    }

    // The optional response should be an array of exactly 1 element (because the 0-element case
    // corresponds to a non-OK error code, which has just been dealt with).
    let rsp = match opt_response {
        cbor::value::Value::Array(mut a) if a.len() == 1 => a.remove(0),
        _ => {
            error!("HAL: failed to parse response data structure!");
            return kmr_common::cbor_type_error(&opt_response, "arr of len 1").map_err(failed_cbor);
        }
    };

    // The response is expected to be an array of 2 elements: a op_type code and an encoded response
    // structure.  The op_type code indicates the type of response structure, which should be what
    // we expect.
    let mut inner_rsp_array = match rsp {
        cbor::value::Value::Array(a) if a.len() == 2 => a,
        _ => {
            error!("HAL: failed to parse inner response data structure!");
            return kmr_common::cbor_type_error(&rsp, "arr of len 2").map_err(failed_cbor);
        }
    };
    let inner_rsp = inner_rsp_array.remove(1);
    let op_type =
        <KeyMintOperation>::from_cbor_value(inner_rsp_array.remove(0)).map_err(failed_cbor)?;
    if op_type != <S>::CODE {
        error!("HAL: inner response data for unexpected opcode {:?}!", op_type);
        return Err(failed_cbor(CborError::UnexpectedItem("wrong ret code", "rsp ret code")));
    }

    <S>::from_cbor_value(inner_rsp).map_err(failed_cbor)
}

/// Abstraction of a HAL service that uses an underlying [`SerializedChannel`] to communicate with
/// an associated TA.
trait ChannelHalService<T: SerializedChannel> {
    /// Return the underlying channel.
    fn channel(&self) -> MutexGuard<T>;

    /// Execute the given request, by serializing it and sending it down the internal channel.  Then
    /// read and deserialize the response.
    fn execute<R, S>(&self, req: R) -> binder::Result<S>
    where
        R: AsCborValue + Code<KeyMintOperation>,
        S: AsCborValue + Code<KeyMintOperation>,
    {
        channel_execute(self.channel().deref_mut(), req)
    }
}

/// Let the TA know information about the userspace environment.
pub fn send_hal_info<T: SerializedChannel>(channel: &mut T) -> binder::Result<()> {
    let req = env::populate_hal_info().map_err(|e| {
        binder::Status::new_exception(
            binder::ExceptionCode::BAD_PARCELABLE,
            Some(&CString::new(format!("failed to determine HAL environment: {}", e)).unwrap()),
        )
    })?;
    info!("HAL->TA: environment info is {:?}", req);
    let _rsp: kmr_common::wire::SetHalInfoResponse = channel_execute(channel, req)?;
    Ok(())
}

/// Let the TA know information about the boot environment.
pub fn send_boot_info<T: SerializedChannel>(
    channel: &mut T,
    req: kmr_common::wire::SetBootInfoRequest,
) -> binder::Result<()> {
    info!("boot->TA: boot info is {:?}", req);
    let _rsp: kmr_common::wire::SetBootInfoResponse = channel_execute(channel, req)?;
    Ok(())
}

/// Provision the TA with attestation ID information.
pub fn send_attest_ids<T: SerializedChannel>(
    channel: &mut T,
    ids: kmr_common::wire::AttestationIdInfo,
) -> binder::Result<()> {
    let req = kmr_common::wire::SetAttestationIdsRequest { ids };
    info!("provision->attestation IDs are {:?}", req);
    let _rsp: kmr_common::wire::SetAttestationIdsResponse = channel_execute(channel, req)?;
    Ok(())
}

/// Let the TA know that early boot has ended
pub fn early_boot_ended<T: SerializedChannel>(channel: &mut T) -> binder::Result<()> {
    info!("boot->TA: early boot ended");
    let req = kmr_common::wire::EarlyBootEndedRequest {};
    let _rsp: kmr_common::wire::EarlyBootEndedResponse = channel_execute(channel, req)?;
    Ok(())
}