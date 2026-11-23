pub const MAX_USAGE_MODEL_BYTES: usize = 512;

/// Model checks consume usage but do not belong to conversations or performance samples.
pub const MODEL_TEST_OPERATIONS: [&str; 3] = ["model_test", "vision_test", "audio_test"];
