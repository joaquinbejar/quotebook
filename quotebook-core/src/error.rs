//! The program's error enum, shared with clients so they can decode custom error codes.

use core::fmt;

/// Why a quotebook operation failed.
///
/// `#[repr(u32)]` with stable discriminants starting at 100 (the matchbook convention). The
/// program maps each variant to `ProgramError::Custom(code)`; [`QuotebookError::from_code`]
/// is the inverse for clients. New variants are only ever appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum QuotebookError {
    /// Unknown instruction discriminator.
    InvalidInstruction = 100,
    /// Instruction data of the wrong length or with invalid values.
    InvalidInstructionData = 101,
    /// A required signer did not sign.
    MissingSignature = 102,
    /// An account is not owned by the expected program.
    InvalidAccountOwner = 103,
    /// An account address does not match its PDA seeds.
    InvalidPda = 104,
    /// A mint does not match the series.
    InvalidMint = 105,
    /// A token account has the wrong owner or mint.
    InvalidVaultOrTokenAccount = 106,
    /// A sysvar account has the wrong address.
    InvalidSysvar = 107,
    /// No ed25519 precompile instruction at the expected index.
    MissingSigVerify = 108,
    /// The precompile instruction holds a signature count other than one, or offsets out of
    /// range.
    MalformedSigVerify = 109,
    /// A precompile instruction index is not `0xFFFF`.
    SigVerifyNotSelfContained = 110,
    /// The signed public key is not `quote.maker`.
    SignerMismatch = 111,
    /// The signed message is not exactly `Quote::LEN` bytes.
    QuoteLengthMismatch = 112,
    /// The quote's domain tag is not `QUOTE_DOMAIN`.
    WrongDomain = 113,
    /// The quote was signed for another program.
    WrongProgram = 114,
    /// The quote was signed for another series or deployment.
    WrongSeries = 115,
    /// The quote is bound to another taker.
    WrongTaker = 116,
    /// The quote expired.
    QuoteExpired = 117,
    /// The quote's lifetime exceeds the series' maximum, or it was issued in the future.
    QuoteTtlTooLong = 118,
    /// The nonce is below the maker's last `cancel_all` watermark.
    NonceCancelled = 119,
    /// The nonce was already filled or cancelled.
    NonceUsed = 120,
    /// The nonce fell below the replay window as later nonces slid it forward.
    NonceOutOfWindow = 121,
    /// The fill size is zero, above the quoted size or below the series minimum.
    FillSizeInvalid = 122,
    /// A size is not on the series' lot grid.
    SizeNotOnLot = 123,
    /// An amount does not fit its integer type.
    Overflow = 124,
    /// The series is paused.
    SeriesPaused = 125,
    /// The signer is not the configured authority.
    NotAuthority = 126,
    /// A Token-2022 mint or account carries an unsupported extension.
    MintExtensionUnsupported = 127,
    /// The quote bytes are not canonical: a side other than 0 or 1, non-zero padding, or a
    /// zero price or size.
    MalformedQuote = 128,
    /// `cancel_all` did not raise the nonce watermark.
    WatermarkNotIncreasing = 129,
}

impl QuotebookError {
    /// Every variant, in discriminant order.
    pub const ALL: [Self; 30] = [
        Self::InvalidInstruction,
        Self::InvalidInstructionData,
        Self::MissingSignature,
        Self::InvalidAccountOwner,
        Self::InvalidPda,
        Self::InvalidMint,
        Self::InvalidVaultOrTokenAccount,
        Self::InvalidSysvar,
        Self::MissingSigVerify,
        Self::MalformedSigVerify,
        Self::SigVerifyNotSelfContained,
        Self::SignerMismatch,
        Self::QuoteLengthMismatch,
        Self::WrongDomain,
        Self::WrongProgram,
        Self::WrongSeries,
        Self::WrongTaker,
        Self::QuoteExpired,
        Self::QuoteTtlTooLong,
        Self::NonceCancelled,
        Self::NonceUsed,
        Self::NonceOutOfWindow,
        Self::FillSizeInvalid,
        Self::SizeNotOnLot,
        Self::Overflow,
        Self::SeriesPaused,
        Self::NotAuthority,
        Self::MintExtensionUnsupported,
        Self::MalformedQuote,
        Self::WatermarkNotIncreasing,
    ];

    /// The custom program error code.
    #[must_use]
    #[inline]
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The variant for a custom program error code, or `None` if the code is not quotebook's.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.code() == code)
    }
}

impl fmt::Display for QuotebookError {
    #[inline(never)]
    #[cold]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::InvalidInstruction => "unknown instruction",
            Self::InvalidInstructionData => "invalid instruction data",
            Self::MissingSignature => "a required signer did not sign",
            Self::InvalidAccountOwner => "account has the wrong owner program",
            Self::InvalidPda => "account address does not match its PDA seeds",
            Self::InvalidMint => "mint does not match the series",
            Self::InvalidVaultOrTokenAccount => "token account has the wrong owner or mint",
            Self::InvalidSysvar => "sysvar account has the wrong address",
            Self::MissingSigVerify => "no ed25519 instruction at the expected index",
            Self::MalformedSigVerify => "malformed ed25519 instruction",
            Self::SigVerifyNotSelfContained => "ed25519 instruction is not self-contained",
            Self::SignerMismatch => "signed public key is not the quote's maker",
            Self::QuoteLengthMismatch => "signed message has the wrong length for a quote",
            Self::WrongDomain => "quote domain tag mismatch",
            Self::WrongProgram => "quote signed for another program",
            Self::WrongSeries => "quote signed for another series or deployment",
            Self::WrongTaker => "quote bound to another taker",
            Self::QuoteExpired => "quote expired",
            Self::QuoteTtlTooLong => "quote lifetime too long or issued in the future",
            Self::NonceCancelled => "nonce cancelled by cancel_all",
            Self::NonceUsed => "nonce already used",
            Self::NonceOutOfWindow => "nonce below the replay window",
            Self::FillSizeInvalid => "invalid fill size",
            Self::SizeNotOnLot => "size not on the lot grid",
            Self::Overflow => "amount overflow",
            Self::SeriesPaused => "series paused",
            Self::NotAuthority => "signer is not the authority",
            Self::MintExtensionUnsupported => "unsupported Token-2022 extension",
            Self::MalformedQuote => "quote bytes are not canonical",
            Self::WatermarkNotIncreasing => "cancel_all must raise the nonce watermark",
        };
        f.write_str(text)
    }
}

impl core::error::Error for QuotebookError {}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]
mod tests {
    extern crate std;

    use super::QuotebookError;
    use std::string::ToString;

    #[test]
    fn test_error_codes_are_dense_from_100() {
        for (i, e) in QuotebookError::ALL.iter().enumerate() {
            assert_eq!(e.code(), 100 + i as u32, "{e:?}");
        }
    }

    #[test]
    fn test_error_from_code_round_trips() {
        for e in QuotebookError::ALL {
            assert_eq!(QuotebookError::from_code(e.code()), Some(e));
        }
        assert_eq!(QuotebookError::from_code(99), None);
        assert_eq!(QuotebookError::from_code(130), None);
    }

    #[test]
    fn test_error_display_is_not_empty() {
        for e in QuotebookError::ALL {
            assert!(!e.to_string().is_empty());
        }
    }
}
