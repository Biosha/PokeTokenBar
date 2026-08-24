//! Per-model token pricing (USD/token). A faithful port of `ModelPricing`: an exact-match
//! table (reverse-engineered to match ccusage offline rates) plus family fallbacks, and 0
//! for unprienced models (never a fake estimate).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl ModelRate {
    pub const ZERO: ModelRate = ModelRate {
        input: 0.0,
        output: 0.0,
        cache_write: 0.0,
        cache_read: 0.0,
    };

    /// Declare rates in USD per **million** tokens for readability; store per-token.
    const fn per_million(input: f64, output: f64, cache_write: f64, cache_read: f64) -> ModelRate {
        ModelRate {
            input: input / 1e6,
            output: output / 1e6,
            cache_write: cache_write / 1e6,
            cache_read: cache_read / 1e6,
        }
    }
}

pub struct ModelPricing;

impl ModelPricing {
    pub fn rate(model: &str) -> ModelRate {
        // Exact table (USD/Mtok).
        match model {
            "claude-opus-4-8" | "claude-opus-4-7" => {
                return ModelRate::per_million(5.0, 25.0, 6.25, 0.5)
            }
            "claude-sonnet-4-6" => return ModelRate::per_million(3.0, 15.0, 3.75, 0.3),
            "claude-haiku-4-5-20251001" => return ModelRate::per_million(1.0, 5.0, 1.25, 0.1),
            "claude-fable-5" => return ModelRate::ZERO,
            "gpt-5.5" => return ModelRate::per_million(5.0, 30.0, 0.0, 0.5),
            "gemini-2.5-pro" => return ModelRate::per_million(1.25, 10.0, 0.0, 0.3125),
            "gemini-2.5-flash" => return ModelRate::per_million(0.30, 2.5, 0.0, 0.075),
            "gemini-2.0-flash" => return ModelRate::per_million(0.10, 0.4, 0.0, 0.025),
            _ => {}
        }

        let m = model.to_ascii_lowercase();
        // Interrupt before the GPT family fallback so `grok-*` doesn't show a fake price.
        if m.starts_with("grok") {
            return ModelRate::ZERO;
        }
        // Flat-rate subscription: the CLI prefixes model names and reports no amount.
        if m.starts_with("antigravity/") {
            return ModelRate::ZERO;
        }
        if m.contains("opus") {
            return ModelRate::per_million(5.0, 25.0, 6.25, 0.5);
        }
        if m.contains("sonnet") {
            return ModelRate::per_million(3.0, 15.0, 3.75, 0.3);
        }
        if m.contains("haiku") {
            return ModelRate::per_million(1.0, 5.0, 1.25, 0.1);
        }
        if m.contains("gpt") || m.contains("codex") || m.contains("o4") || m.contains("o3") {
            return ModelRate::per_million(5.0, 30.0, 0.0, 0.5);
        }
        if m.starts_with("gemini") {
            if m.contains("pro") {
                return ModelRate::per_million(1.25, 10.0, 0.0, 0.3125);
            }
            if m.contains("flash") {
                return ModelRate::per_million(0.30, 2.5, 0.0, 0.075);
            }
        }
        ModelRate::ZERO
    }

    pub fn cost(model: &str, input: i64, output: i64, cache_write: i64, cache_read: i64) -> f64 {
        let r = Self::rate(model);
        input as f64 * r.input
            + output as f64 * r.output
            + cache_write as f64 * r.cache_write
            + cache_read as f64 * r.cache_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_table_match() {
        let r = ModelPricing::rate("claude-sonnet-4-6");
        assert!((r.input - 3.0 / 1e6).abs() < 1e-12);
        assert!((r.output - 15.0 / 1e6).abs() < 1e-12);
    }

    #[test]
    fn family_fallback() {
        // Version drift falls back to the family's table rate.
        assert_eq!(
            ModelPricing::rate("claude-opus-4-9"),
            ModelPricing::rate("claude-opus-4-8")
        );
        assert_eq!(
            ModelPricing::rate("claude-sonnet-5"),
            ModelPricing::rate("claude-sonnet-4-6")
        );
    }

    #[test]
    fn grok_is_zero() {
        assert_eq!(ModelPricing::rate("grok-4o-latest"), ModelRate::ZERO);
    }

    #[test]
    fn cost_sums_components() {
        // 1M output sonnet = $15.
        let c = ModelPricing::cost("claude-sonnet-4-6", 0, 1_000_000, 0, 0);
        assert!((c - 15.0).abs() < 1e-6);
    }
}
