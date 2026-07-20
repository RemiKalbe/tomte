//! Re-render verification: the runtime twin of the tests' round-trip invariant.

use crate::chezmoi::{ChezmoiClient, ChezmoiError};

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error(transparent)]
    Chezmoi(#[from] ChezmoiError),
    #[error("re-rendered template does not match the resolved text")]
    Mismatch { expected: Vec<u8>, actual: Vec<u8> },
}

pub fn verify_write_back(
    chezmoi: &ChezmoiClient,
    new_template: &str,
    expected: &str,
) -> Result<(), VerifyError> {
    let actual = chezmoi.execute_template(new_template.as_bytes())?;
    if actual != expected.as_bytes() {
        return Err(VerifyError::Mismatch {
            expected: expected.as_bytes().to_vec(),
            actual,
        });
    }
    Ok(())
}
