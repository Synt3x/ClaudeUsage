//! Encrypting the claude.ai session key on disk with Windows' per-user encryption (DPAPI).
//! Only the same Windows user on the same PC can decrypt it, so a copied settings.json
//! doesn't give away the account.

use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

fn take(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let out = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe { LocalFree(blob.pbData as _) };
    out
}

/// Encrypted, hex-encoded text; None if Windows couldn't encrypt it.
pub fn protect(text: &str) -> Option<String> {
    let mut data = text.as_bytes().to_vec();
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    let ok = unsafe { CryptProtectData(&input, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
    if ok == 0 {
        return None;
    }
    Some(take(output).iter().map(|b| format!("{b:02x}")).collect())
}

/// The text back from `protect`; None if it can't be decrypted (another user or PC).
pub fn unprotect(hex: &str) -> Option<String> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let mut data: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<_, _>>().ok()?;
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_mut_ptr() };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    let ok = unsafe { CryptUnprotectData(&input, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output) };
    if ok == 0 {
        return None;
    }
    String::from_utf8(take(output)).ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let enc = super::protect("not-a-real-key").unwrap();
        assert!(!enc.contains("not-a-real-key"));
        assert_eq!(super::unprotect(&enc).as_deref(), Some("not-a-real-key"));
        assert_eq!(super::unprotect("zz"), None);
    }
}
