//! Provider registry: build provider impls from config.
pub mod openai;
pub mod anthropic;
#[cfg(test)]
pub mod test_mock_upstream;
