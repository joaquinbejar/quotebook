# QuoteBook

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Request-for-quote trading on Solana with quotes signed off chain.

A maker signs a quote off chain; the taker submits it, and the program verifies the ed25519 signature through instruction introspection, checks expiry and nonce, and settles atomically. Cancelling every outstanding quote is one nonce bump.

**Status:** design phase, no code yet.
