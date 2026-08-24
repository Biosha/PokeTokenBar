//! Pokémon natures (port of `PokemonNature`): 25 kinds, fixed at hatch, identity-only
//! (no stat effect in this app). The variant order matches Swift's `CaseIterable`, so
//! `from_index(roll % 25)` reproduces the original roll exactly.

use crate::i18n::Language;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Nature {
    Hardy,
    Lonely,
    Brave,
    Adamant,
    Naughty,
    Bold,
    Docile,
    Relaxed,
    Impish,
    Lax,
    Timid,
    Hasty,
    Serious,
    Jolly,
    Naive,
    Modest,
    Mild,
    Quiet,
    Bashful,
    Rash,
    Calm,
    Gentle,
    Sassy,
    Careful,
    Quirky,
}

impl Nature {
    /// All 25 natures in canonical (Swift `CaseIterable`) order.
    pub const ALL: [Nature; 25] = [
        Nature::Hardy,
        Nature::Lonely,
        Nature::Brave,
        Nature::Adamant,
        Nature::Naughty,
        Nature::Bold,
        Nature::Docile,
        Nature::Relaxed,
        Nature::Impish,
        Nature::Lax,
        Nature::Timid,
        Nature::Hasty,
        Nature::Serious,
        Nature::Jolly,
        Nature::Naive,
        Nature::Modest,
        Nature::Mild,
        Nature::Quiet,
        Nature::Bashful,
        Nature::Rash,
        Nature::Calm,
        Nature::Gentle,
        Nature::Sassy,
        Nature::Careful,
        Nature::Quirky,
    ];

    /// Index-based pick (mint rolls use `ALL[roll % 25]` filtered to "≠ current").
    pub fn from_index(i: u64) -> Nature {
        Self::ALL[(i % 25) as usize]
    }

    /// Official translated name (port of `PokemonNature.name(_:)`).
    pub fn name(self, lang: Language) -> &'static str {
        let names: (&'static str, &'static str, &'static str, &'static str) = match self {
            Nature::Hardy => ("노력", "Hardy", "がんばりや", "Fuerte"),
            Nature::Lonely => ("외로움", "Lonely", "さみしがり", "Huraña"),
            Nature::Brave => ("용감", "Brave", "ゆうかん", "Audaz"),
            Nature::Adamant => ("고집", "Adamant", "いじっぱり", "Firme"),
            Nature::Naughty => ("개구쟁이", "Naughty", "やんちゃ", "Pícara"),
            Nature::Bold => ("대담", "Bold", "ずぶとい", "Osada"),
            Nature::Docile => ("온순", "Docile", "すなお", "Dócil"),
            Nature::Relaxed => ("무사태평", "Relaxed", "のんき", "Plácida"),
            Nature::Impish => ("장난꾸러기", "Impish", "わんぱく", "Agitada"),
            Nature::Lax => ("촐랑", "Lax", "のうてんき", "Floja"),
            Nature::Timid => ("겁쟁이", "Timid", "おくびょう", "Miedosa"),
            Nature::Hasty => ("성급", "Hasty", "せっかち", "Activa"),
            Nature::Serious => ("성실", "Serious", "まじめ", "Seria"),
            Nature::Jolly => ("명랑", "Jolly", "ようき", "Alegre"),
            Nature::Naive => ("천진난만", "Naive", "むじゃき", "Ingenua"),
            Nature::Modest => ("조심", "Modest", "ひかえめ", "Modesta"),
            Nature::Mild => ("의젓", "Mild", "おっとり", "Afable"),
            Nature::Quiet => ("냉정", "Quiet", "れいせい", "Mansa"),
            Nature::Bashful => ("수줍음", "Bashful", "てれや", "Tímida"),
            Nature::Rash => ("덜렁", "Rash", "うっかりや", "Alocada"),
            Nature::Calm => ("차분", "Calm", "おだやか", "Serena"),
            Nature::Gentle => ("얌전", "Gentle", "おとなしい", "Amable"),
            Nature::Sassy => ("건방", "Sassy", "なまいき", "Grosera"),
            Nature::Careful => ("신중", "Careful", "しんちょう", "Cauta"),
            Nature::Quirky => ("변덕", "Quirky", "きまぐれ", "Rara"),
        };
        match lang {
            Language::Ko => names.0,
            Language::En => names.1,
            Language::Ja => names.2,
            Language::Es => names.3,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_25_natures_distinct_and_indexed() {
        assert_eq!(Nature::ALL.len(), 25);
        let mut seen = std::collections::HashSet::new();
        for n in Nature::ALL {
            assert!(seen.insert(n));
            assert_eq!(Nature::from_index(Nature::ALL.iter().position(|m| *m == n).unwrap() as u64), n);
        }
    }

    #[test]
    fn names_translated_in_all_languages() {
        assert_eq!(Nature::Hardy.name(Language::En), "Hardy");
        assert_eq!(Nature::Hardy.name(Language::Ko), "노력");
        assert_eq!(Nature::Hardy.name(Language::Ja), "がんばりや");
        assert_eq!(Nature::Hardy.name(Language::Es), "Fuerte");
        assert_eq!(Nature::Quirky.name(Language::Ja), "きまぐれ");
        // Every nature has a non-empty name in every language.
        for n in Nature::ALL {
            for lang in [Language::En, Language::Ko, Language::Ja, Language::Es] {
                assert!(!n.name(lang).is_empty(), "{n:?} {lang:?}");
            }
        }
    }
}
