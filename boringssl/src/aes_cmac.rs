//! BoringSSL-based implementation of AES-CMAC.
use crate::types::CmacCtx;
use crate::{malloc_err, openssl_last_err};
use alloc::boxed::Box;
use alloc::vec::Vec;
use bssl_ffi as ffi;
use kmr_common::{crypto, crypto::OpaqueOr, explicit, km_err, vec_try, Error};
use log::error;

/// [`crypto::AesCmac`] implementation based on BoringSSL.
pub struct BoringAesCmac;

impl crypto::AesCmac for BoringAesCmac {
    fn begin(
        &self,
        key: OpaqueOr<crypto::aes::Key>,
    ) -> Result<Box<dyn crypto::AccumulatingOperation>, Error> {
        let key = explicit!(key)?;
        // Safety: all of the `ffi::EVP_aes_<N>_cbc` functions return a non-null valid pointer.
        let (cipher, k) = unsafe {
            match &key {
                crypto::aes::Key::Aes128(k) => (ffi::EVP_aes_128_cbc(), &k[..]),
                crypto::aes::Key::Aes192(k) => (ffi::EVP_aes_192_cbc(), &k[..]),
                crypto::aes::Key::Aes256(k) => (ffi::EVP_aes_256_cbc(), &k[..]),
            }
        };

        let op = BoringAesCmacOperation {
            // Safety: raw pointer is immediately checked for null below, and BoringSSL only emits
            // valid pointers or null.
            ctx: unsafe { CmacCtx(ffi::CMAC_CTX_new()) },
        };
        if op.ctx.0.is_null() {
            return Err(malloc_err!());
        }

        // Safety: `op.ctx` is known non-null and valid, as is `cipher`.  `key_len` is length of
        // `key.0`, which is a valid `Vec<u8>`.
        let result = unsafe {
            ffi::CMAC_Init(
                op.ctx.0,
                k.as_ptr() as *const libc::c_void,
                k.len(),
                cipher,
                core::ptr::null_mut(),
            )
        };
        if result != 1 {
            error!("Failed to CMAC_Init()");
            return Err(openssl_last_err());
        }
        Ok(Box::new(op))
    }
}

/// [`crypto::AesCmacOperation`] implementation based on BoringSSL.
///
/// This implementation uses the `unsafe` wrappers around `CMAC_*` functions directly, because
/// BoringSSL does not support the `EVP_PKEY_CMAC` implementations that are used in the rust-openssl
/// crate.
pub struct BoringAesCmacOperation {
    // Safety: `ctx` is always non-null and valid except for initial error path in `begin()`
    ctx: CmacCtx,
}

impl core::ops::Drop for BoringAesCmacOperation {
    fn drop(&mut self) {
        // Safety: `self.ctx` might be null (in the error path when `ffi::CMAC_CTX_new` fails)
        // but `ffi::CMAC_CTX_free` copes with null.
        unsafe {
            ffi::CMAC_CTX_free(self.ctx.0);
        }
    }
}

impl crypto::AccumulatingOperation for BoringAesCmacOperation {
    fn update(&mut self, data: &[u8]) -> Result<(), Error> {
        // Safety: `self.ctx` is non-null and valid, and `data` is a valid slice.
        let result = unsafe { ffi::CMAC_Update(self.ctx.0, data.as_ptr(), data.len()) };
        if result != 1 {
            return Err(openssl_last_err());
        }
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<Vec<u8>, Error> {
        let mut output_len: usize = crypto::aes::BLOCK_SIZE;
        let mut output = vec_try![0; crypto::aes::BLOCK_SIZE]?;
        // Safety: `self.ctx` is non-null and valid; `output_len` is correct size of `output`
        // buffer.
        let result = unsafe {
            ffi::CMAC_Final(self.ctx.0, output.as_mut_ptr(), &mut output_len as *mut usize)
        };
        if result != 1 {
            return Err(openssl_last_err());
        }
        if output_len != crypto::aes::BLOCK_SIZE {
            return Err(km_err!(BoringSslError, "Unexpected CMAC output size of {}", output_len));
        }
        Ok(output)
    }
}
