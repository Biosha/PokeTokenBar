//! Companion/user-facing strings in the four languages the macOS app ships
//! (`Localization.swift`: ko/en/ja/es), default English.
//!
//! Scope: the companion, Pokédex, bag, shop and notification strings used by the core and
//! the headless CLI. Pure app-chrome strings (settings pages, Keychain, updates, save
//! transfer) land with the Phase 2 GUI. Species names are NOT here — they come from the
//! static pool table (`pool::localized_name`), exactly as on macOS (PokéAPI names).

use crate::companion::{ItemKind, Rarity};
use serde::{Deserialize, Serialize};

/// App language (port of `AppLanguage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Ko,
    En,
    Ja,
    Es,
}

impl Language {
    pub const ALL: [Language; 4] = [Language::Ko, Language::En, Language::Ja, Language::Es];

    /// Parse a language code ("ko"/"en"/"ja"/"es", case-insensitive).
    pub fn from_code(code: &str) -> Option<Language> {
        match code.trim().to_ascii_lowercase().as_str() {
            "ko" => Some(Language::Ko),
            "en" => Some(Language::En),
            "ja" => Some(Language::Ja),
            "es" => Some(Language::Es),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Language::Ko => "ko",
            Language::En => "en",
            Language::Ja => "ja",
            Language::Es => "es",
        }
    }

    /// Native name, as shown in language pickers (`AppLanguage.label`).
    pub fn label(self) -> &'static str {
        match self {
            Language::Ko => "한국어",
            Language::En => "English",
            Language::Ja => "日本語",
            Language::Es => "Español",
        }
    }
}

/// Resolve the effective language: `$PTB_LANG` env override → companion state → config →
/// English. (macOS persists the choice in the companion state; the env var is a headless
/// diagnostic, and `Config::load().language` is the config-file setting.)
pub fn resolve_language(state_language: &str, config_language: &str) -> Language {
    if let Ok(env) = std::env::var("PTB_LANG") {
        if let Some(l) = Language::from_code(&env) {
            return l;
        }
    }
    if let Some(l) = Language::from_code(state_language) {
        return l;
    }
    if let Some(l) = Language::from_code(config_language) {
        return l;
    }
    Language::En
}

/// Localized string access (port of the `L` struct). Values are static per language.
#[derive(Debug, Clone, Copy)]
pub struct L(Language);

impl L {
    pub fn new(lang: Language) -> Self {
        L(lang)
    }

    pub fn lang(&self) -> Language {
        self.0
    }

    fn t(
        &self,
        ko: &'static str,
        en: &'static str,
        ja: &'static str,
        es: &'static str,
    ) -> &'static str {
        match self.0 {
            Language::Ko => ko,
            Language::En => en,
            Language::Ja => ja,
            Language::Es => es,
        }
    }

    // MARK: companion state line

    pub fn status_egg(&self) -> &'static str {
        self.t(
            "곧 깨어나요.",
            "Hatching soon.",
            "もうすぐ孵化します。",
            "Está a punto de eclosionar.",
        )
    }
    pub fn status_idle(&self) -> &'static str {
        self.t(
            "오늘은 조용히 자리를 지켜요.",
            "Keeping quiet today.",
            "今日は静かにしています。",
            "Hoy se mantiene tranquilo.",
        )
    }
    pub fn status_working(&self) -> &'static str {
        self.t(
            "오늘의 작업 흔적이 쌓이고 있어요.",
            "Today's work is piling up.",
            "本日の作業が積み重なっています。",
            "El trabajo de hoy se va acumulando.",
        )
    }
    pub fn status_focus(&self) -> &'static str {
        self.t(
            "지금은 집중 모드예요.",
            "In focus mode now.",
            "今は集中モードです。",
            "Ahora está en modo concentración.",
        )
    }
    pub fn status_tired(&self) -> &'static str {
        self.t(
            "한도에 가까워요. 잠깐 쉬어도 괜찮아요.",
            "Close to the limit. A short break is fine.",
            "上限が近いです。少し休んでも大丈夫。",
            "Está cerca del límite. Un pequeño descanso no vendría mal.",
        )
    }
    pub fn status_sleep(&self) -> &'static str {
        self.t(
            "지금은 자고 있어요.",
            "Sleeping now.",
            "今は眠っています。",
            "Ahora está durmiendo.",
        )
    }
    pub fn status_evolved(&self, name: &str) -> String {
        let s = self.t(
            "{name}(으)로 진화했어요!",
            "Evolved into {name}!",
            "{name} に進化しました！",
            "¡Evolucionó a {name}!",
        );
        s.replace("{name}", name)
    }
    pub fn status_grew(&self) -> &'static str {
        self.t("성장했어요!", "It grew!", "成長しました！", "¡Ha crecido!")
    }

    // MARK: stage / egg copy

    pub fn final_form(&self) -> &'static str {
        self.t("최종 진화체", "Final form", "最終進化", "Forma final")
    }
    pub fn stage(&self, i: i64, k: i64) -> String {
        let s = self.t(
            "진화 단계 {i} / {k}",
            "Stage {i} / {k}",
            "進化段階 {i} / {k}",
            "Etapa {i} / {k}",
        );
        s.replace("{i}", &i.to_string())
            .replace("{k}", &k.to_string())
    }
    pub fn unknown_next_evolution(&self) -> &'static str {
        self.t(
            "알 수 없는 다음 진화",
            "Unknown next evolution",
            "次の進化先は不明",
            "Próxima evolución desconocida",
        )
    }
    pub fn egg_incubating(&self) -> &'static str {
        self.t(
            "🥚 부화 준비 중",
            "🥚 Incubating",
            "🥚 孵化の準備中",
            "🥚 Incubando",
        )
    }
    pub fn egg_to_hatch(&self, amount: &str) -> String {
        let s = self.t(
            "부화까지 {amount}",
            "{amount} to hatch",
            "孵化まで {amount}",
            "{amount} para eclosionar",
        );
        s.replace("{amount}", amount)
    }
    pub fn to_next_evolution(&self, amount: &str) -> String {
        let s = self.t(
            "다음 진화까지 {amount}",
            "{amount} to next evolution",
            "次の進化まで {amount}",
            "{amount} para la siguiente evolución",
        );
        s.replace("{amount}", amount)
    }
    pub fn to_graduation(&self, amount: &str) -> String {
        let s = self.t(
            "졸업까지 {amount}",
            "{amount} to graduation",
            "卒業まで {amount}",
            "{amount} para graduarse",
        );
        s.replace("{amount}", amount)
    }
    pub fn graduated(&self, name: &str) -> String {
        let s = self.t(
            "{name} 졸업 → 도감에 보존. 새 Token Egg가 도착했어요!",
            "{name} graduated → saved to the dex. A new Token Egg has arrived!",
            "{name} 卒業 → 図鑑に保存。新しいToken Eggが届きました！",
            "{name} se graduó → guardado en la Pokédex. ¡Ha llegado un nuevo Token Egg!",
        );
        s.replace("{name}", name)
    }
    pub fn egg_imminent(&self) -> &'static str {
        self.t(
            "곧 부화해요!",
            "About to hatch!",
            "もうすぐ孵化！",
            "¡Está a punto de eclosionar!",
        )
    }
    pub fn egg_first_run_hint(&self) -> &'static str {
        self.t(
            "로컬 AI 코딩 도구의 사용량으로 자라요. 약 5M 토큰을 쓰면 알이 부화해요.",
            "Grows from your local AI coding usage. Your egg hatches after ~5M tokens.",
            "ローカルの AI コーディング使用量で育ちます。約5Mトークンでタマゴが孵化します。",
            "Crece con el uso de tus herramientas locales de programación con IA. Tu huevo eclosiona tras unos 5M de tokens.",
        )
    }

    // MARK: dex

    pub fn dex_title(&self) -> &'static str {
        self.t("도감", "Pokédex", "図鑑", "Pokédex")
    }
    pub fn dex_total(&self, n: i64) -> String {
        let s = self.t("총 {n}마리", "{n} total", "全{n}匹", "{n} en total");
        s.replace("{n}", &n.to_string())
    }
    pub fn catch_log_title(&self) -> &'static str {
        self.t("포획 로그", "Catch log", "捕獲ログ", "Registro de capturas")
    }
    pub fn dex_species_total(&self, n: i64) -> String {
        let s = self.t("{n}종", "{n} species", "{n}種", "{n} especies");
        s.replace("{n}", &n.to_string())
    }
    pub fn dex_raising(&self) -> &'static str {
        self.t("키우는 중", "Raising", "育成中", "Criando")
    }
    pub fn dex_empty_title(&self) -> &'static str {
        self.t(
            "아직 잡은 포켓몬이 없어요!",
            "No Pokémon caught yet!",
            "まだ捕まえたポケモンがいません！",
            "¡Todavía no has capturado ningún Pokémon!",
        )
    }
    pub fn dex_empty_hint(&self) -> &'static str {
        self.t(
            "토큰을 써서 첫 포켓몬을 부화시켜 보세요.",
            "Spend tokens to hatch your first Pokémon.",
            "トークンを使って最初のポケモンを孵化させましょう。",
            "Usa tokens para eclosionar tu primer Pokémon.",
        )
    }
    pub fn dex_shiny_label(&self) -> &'static str {
        self.t("이로치", "Shiny", "色違い", "Variocolor")
    }
    pub fn dex_filter_hint(&self) -> &'static str {
        self.t(
            "탭하면 이 희귀도만 보기 · 다시 탭하면 전체",
            "Tap to show only this rarity · tap again to clear",
            "タップでこの希少度のみ表示・再タップで全体",
            "Toca para ver solo esta rareza · toca de nuevo para ver todo",
        )
    }

    pub fn rarity_common(&self) -> &'static str {
        self.t("일반", "Common", "ノーマル", "Común")
    }
    pub fn rarity_uncommon(&self) -> &'static str {
        self.t("고급", "Uncommon", "アンコモン", "Poco común")
    }
    pub fn rarity_rare(&self) -> &'static str {
        self.t("희귀", "Rare", "レア", "Raro")
    }
    pub fn rarity_legendary(&self) -> &'static str {
        self.t("전설", "Legendary", "伝説", "Legendario")
    }
    pub fn rarity_label(&self, r: Rarity) -> &'static str {
        match r {
            Rarity::Common => self.rarity_common(),
            Rarity::Uncommon => self.rarity_uncommon(),
            Rarity::Rare => self.rarity_rare(),
            Rarity::Legendary => self.rarity_legendary(),
        }
    }

    // MARK: notifications

    pub fn notif_hatch_title(&self) -> &'static str {
        self.t("🥚 부화!", "🥚 Hatched!", "🥚 孵化！", "🥚 ¡Eclosionó!")
    }
    pub fn notif_hatch_body(&self, name: &str) -> String {
        let s = self.t(
            "알에서 {name}이(가) 나왔어요!",
            "{name} hatched from the egg!",
            "タマゴから {name} が生まれました！",
            "¡{name} salió del huevo!",
        );
        s.replace("{name}", name)
    }
    pub fn notif_shiny_hatch_title(&self) -> &'static str {
        self.t(
            "✨ 이로치 포켓몬!",
            "✨ Shiny Pokémon!",
            "✨ 色違いポケモン！",
            "✨ ¡Pokémon variocolor!",
        )
    }
    pub fn notif_shiny_hatch_body(&self, name: &str) -> String {
        let s = self.t(
            "이로치 {name}이(가) 태어났어요! (1/64)",
            "A shiny {name} hatched! (1 in 64)",
            "色違いの {name} が生まれました！(1/64)",
            "¡Nació un {name} variocolor! (1 entre 64)",
        );
        s.replace("{name}", name)
    }
    pub fn notif_evolve_title(&self) -> &'static str {
        self.t("✨ 진화!", "✨ Evolved!", "✨ 進化！", "✨ ¡Evolucionó!")
    }
    pub fn notif_evolve_body(&self, name: &str) -> String {
        self.status_evolved(name)
    }
    pub fn notif_ditto_reveal_title(&self) -> &'static str {
        self.t(
            "🎭 어라? 메타몽!",
            "🎭 Huh? It's Ditto!",
            "🎭 あれ？メタモン！",
            "🎭 ¿Eh? ¡Es Ditto!",
        )
    }
    pub fn notif_ditto_reveal_body(&self, disguise: &str) -> String {
        let s = self.t(
            "{disguise}인 줄 알았는데 — 사실은 메타몽이었어요!",
            "You thought it was {disguise} — it was Ditto all along!",
            "{disguise} だと思ってた… 実はメタモンでした！",
            "Pensabas que era {disguise} — ¡en realidad era Ditto!",
        );
        s.replace("{disguise}", disguise)
    }
    pub fn notif_shiny_ditto_reveal_title(&self) -> &'static str {
        self.t(
            "🎭✨ 어라? 이로치 메타몽!",
            "🎭✨ Huh? A shiny Ditto!",
            "🎭✨ あれ？色違いメタモン！",
            "🎭✨ ¿Eh? ¡Un Ditto variocolor!",
        )
    }
    pub fn notif_shiny_ditto_reveal_body(&self, disguise: &str) -> String {
        let s = self.t(
            "{disguise}인 줄 알았는데 — 이로치 메타몽이었어요! (1/64)",
            "You thought it was {disguise} — it was a shiny Ditto! (1 in 64)",
            "{disguise} だと思ってた… 色違いのメタモンでした！(1/64)",
            "Pensabas que era {disguise} — ¡era un Ditto variocolor! (1 entre 64)",
        );
        s.replace("{disguise}", disguise)
    }
    pub fn notif_graduate_title(&self) -> &'static str {
        self.t("🎓 졸업!", "🎓 Graduated!", "🎓 卒業！", "🎓 ¡Graduado!")
    }
    pub fn notif_graduate_body(&self, name: &str) -> String {
        let s = self.t(
            "{name} — 도감에 보존! 새 알이 도착했어요.",
            "{name} — saved to your Pokédex! A new egg has arrived.",
            "{name} — 図鑑に保存！新しいタマゴが届きました。",
            "{name} — ¡guardado en tu Pokédex! Ha llegado un nuevo huevo.",
        );
        s.replace("{name}", name)
    }
    pub fn notif_candy_title(&self, item: &str, count: i64) -> String {
        let s = self.t(
            "🍬 {item} {count}개를 받았어요!",
            "🍬 You got {count}× {item}!",
            "🍬 {item}を{count}個もらいました！",
            "🍬 ¡Has recibido {count}× {item}!",
        );
        s.replace("{item}", item)
            .replace("{count}", &count.to_string())
    }
    pub fn notif_candy_body(&self, window: &str) -> String {
        let s = self.t(
            "{window} 토큰 한도를 다 채웠어요. 열심히 쓴 만큼 사탕을 드려요 — 포켓몬에게 써서 진화시켜 보세요!",
            "You maxed out your {window} token limit. A treat for the effort — use it to evolve your Pokémon!",
            "{window}のトークン上限を使い切りました。がんばったごほうびです — ポケモンに使って進化させよう！",
            "Has agotado tu límite de tokens {window}. Un premio por el esfuerzo — ¡úsalo para evolucionar a tu Pokémon!",
        );
        s.replace("{window}", window)
    }

    // MARK: bag / items

    pub fn bag(&self) -> &'static str {
        self.t("가방", "Bag", "バッグ", "Bolsa")
    }
    pub fn bag_empty_title(&self) -> &'static str {
        self.t(
            "아직 가방이 비어있어요!",
            "Your bag is empty!",
            "バッグはまだ空っぽです！",
            "¡Tu bolsa todavía está vacía!",
        )
    }
    pub fn use_item(&self) -> &'static str {
        self.t("사용하기", "Use", "つかう", "Usar")
    }
    pub fn use_(&self) -> &'static str {
        self.t("사용", "Use", "つかう", "Usar")
    }
    pub fn cancel(&self) -> &'static str {
        self.t("취소", "Cancel", "キャンセル", "Cancelar")
    }
    pub fn use_on_current(&self, name: &str) -> String {
        let s = self.t(
            "{name}에게 사용할까요?",
            "Use on {name}?",
            "{name} に使いますか？",
            "¿Usar en {name}?",
        );
        s.replace("{name}", name)
    }
    pub fn use_after_hatch(&self) -> &'static str {
        self.t(
            "부화 후 사용할 수 있어요",
            "Usable after hatching",
            "孵化後に使えます",
            "Se puede usar después de eclosionar",
        )
    }
    pub fn use_needs_pokemon(&self) -> &'static str {
        self.t(
            "사용할 포켓몬이 없어요",
            "No Pokémon to use it on",
            "使えるポケモンがいません",
            "No hay ningún Pokémon en quien usarlo",
        )
    }
    pub fn mint_effect_hint(&self) -> &'static str {
        self.t(
            "성격 랜덤 변경",
            "Random nature",
            "せいかくランダム変更",
            "Naturaleza aleatoria",
        )
    }

    /// Item display name (official local names, as for species).
    pub fn item_name(&self, kind: ItemKind) -> &'static str {
        match kind {
            ItemKind::RareCandy => {
                self.t("이상한 사탕", "Rare Candy", "ふしぎなアメ", "Caramelo Raro")
            }
            ItemKind::Mint => self.t("민트", "Mint", "ミント", "Menta"),
            ItemKind::ShinyCharm => self.t(
                "이로치 부적",
                "Shiny Charm",
                "ひかるおまもり",
                "Amuleto Iris",
            ),
        }
    }

    /// Item description (the Rare Candy one derives the amount from `RareCandy::XP`,
    /// exactly like `L.itemDescription`).
    pub fn item_description(&self, kind: ItemKind) -> String {
        match kind {
            ItemKind::RareCandy => {
                let xp = compact_tokens(crate::companion::RareCandy::XP);
                let s = self.t(
                    "현재 포켓몬의 경험치를 {xp} 올려줘요.",
                    "Raises your Pokémon's EXP by {xp}.",
                    "ポケモンの経験値を{xp}上げます。",
                    "Aumenta la experiencia de tu Pokémon en {xp}.",
                );
                s.replace("{xp}", &xp)
            }
            ItemKind::Mint => self.t(
                "현재 포켓몬의 성격을 랜덤으로 바꿔줘요.",
                "Randomly changes your Pokémon's nature.",
                "ポケモンのせいかくをランダムに変えます。",
                "Cambia aleatoriamente la naturaleza de tu Pokémon.",
            )
            .to_string(),
            ItemKind::ShinyCharm => self.t(
                "보유하면 이로치 포켓몬이 태어날 확률이 올라가요.",
                "While owned, raises the chance of hatching a shiny.",
                "持っていると色違いが生まれる確率が上がります。",
                "Mientras lo tengas, aumenta la probabilidad de que nazca un Pokémon variocolor.",
            )
            .to_string(),
        }
    }

    // MARK: shop

    pub fn shop(&self) -> &'static str {
        self.t("상점", "Shop", "ショップ", "Tienda")
    }
    pub fn spendable_tokens(&self) -> &'static str {
        self.t(
            "쓸 수 있는 토큰",
            "Spendable tokens",
            "使えるトークン",
            "Tokens disponibles",
        )
    }
    pub fn shop_hint(&self) -> &'static str {
        self.t(
            "사용한 토큰으로 아이템을 살 수 있어요.",
            "Spend the tokens you've used on items.",
            "使ったトークンでアイテムを購入できます。",
            "Usa los tokens que has consumido para comprar objetos.",
        )
    }
    pub fn buy(&self) -> &'static str {
        self.t("구매", "Buy", "購入", "Comprar")
    }
    pub fn buy_confirm(&self, name: &str) -> String {
        let s = self.t(
            "{name} 구매할까요?",
            "Buy {name}?",
            "{name} を購入しますか？",
            "¿Comprar {name}?",
        );
        s.replace("{name}", name)
    }
    pub fn not_enough_tokens(&self) -> &'static str {
        self.t(
            "토큰이 부족해요",
            "Not enough tokens",
            "トークンが足りません",
            "No tienes suficientes tokens",
        )
    }
    pub fn owned_count(&self, n: i64) -> String {
        let s = self.t("보유 ×{n}", "Owned ×{n}", "所持 ×{n}", "En posesión ×{n}");
        s.replace("{n}", &n.to_string())
    }
    pub fn shop_price_label(&self) -> &'static str {
        self.t("가격", "Price", "価格", "Precio")
    }
    pub fn owned_already(&self) -> &'static str {
        self.t("보유 중", "Owned", "所持済み", "En posesión")
    }
    pub fn shiny_charm_effect_hint(&self) -> &'static str {
        self.t(
            "이로치 확률 ↑ · 적용 중",
            "Shiny rate ↑ · active",
            "色違い率↑ · 適用中",
            "Prob. variocolor ↑ · activo",
        )
    }

    /// Egg display name. Written as explicit quadruples — not composed from the rarity
    /// label — because the Japanese particles would not fit (` Localization.eggName`).
    pub fn egg_name(&self, tier: Option<Rarity>) -> &'static str {
        match tier {
            None | Some(Rarity::Common) => self.t(
                "포켓몬 알",
                "Pokémon Egg",
                "ポケモンのタマゴ",
                "Huevo Pokémon",
            ),
            Some(Rarity::Uncommon) => self.t(
                "고급 알",
                "Uncommon Egg",
                "アンコモンのタマゴ",
                "Huevo poco común",
            ),
            Some(Rarity::Rare) => self.t("희귀 알", "Rare Egg", "レアのタマゴ", "Huevo raro"),
            Some(Rarity::Legendary) => self.t(
                "전설 알",
                "Legendary Egg",
                "でんせつのタマゴ",
                "Huevo legendario",
            ),
        }
    }
    pub fn egg_description(&self, tier: Option<Rarity>) -> String {
        let Some(tier) = tier.filter(|t| *t != Rarity::Common) else {
            return self
                .t(
                    "지금 포켓몬을 놓아주고 새 알로 다시 시작해요.",
                    "Send off your current Pokémon and start fresh with a new egg.",
                    "いまのポケモンを手放して新しいタマゴからやり直します。",
                    "Suelta a tu Pokémon actual y empieza de nuevo con un huevo nuevo.",
                )
                .to_string();
        };
        let r = self.rarity_label(tier);
        let s = self.t(
            "지금 포켓몬을 놓아주고 {r} 이상이 확정으로 나오는 알을 받아요.",
            "Send off your current Pokémon for an egg guaranteed to hatch {r} or better.",
            "いまのポケモンを手放して {r} 以上が確定で孵るタマゴをもらいます。",
            "Suelta a tu Pokémon actual y consigue un huevo garantizado de {r} o superior.",
        );
        s.replace("{r}", r)
    }
    pub fn egg_guarantee_hint(&self, tier: Rarity) -> String {
        let r = self.rarity_label(tier);
        let s = self.t(
            "{r} 이상 확정",
            "{r} or better",
            "{r} 以上確定",
            "{r} o superior garantizado",
        );
        s.replace("{r}", r)
    }
    pub fn egg_confirm(&self, mon_name: &str, egg_name: &str) -> String {
        let s = self.t(
            "{mon}(을/를) 놓아주고 {egg}(으)로 바꿀까요?",
            "Send off {mon} for the {egg}?",
            "{mon} を手放して {egg} にしますか？",
            "¿Soltar a {mon} y cambiarlo por {egg}?",
        );
        s.replace("{mon}", mon_name).replace("{egg}", egg_name)
    }
    pub fn fresh_egg_shiny_warning(&self) -> &'static str {
        self.t(
            "⚠️ 이로치 포켓몬이에요! 정말 놓아줄까요?",
            "⚠️ This one is shiny! Really send it off?",
            "⚠️ 色違いです！本当に手放しますか？",
            "⚠️ ¡Este es variocolor! ¿Seguro que quieres soltarlo?",
        )
    }
    pub fn fresh_egg_discard_shiny(&self) -> &'static str {
        self.t(
            "이로치 놓아주기",
            "Send shiny off",
            "手放す",
            "Soltar variocolor",
        )
    }

    // MARK: app chrome (Phase 2 GUI — tab bar, home, limits, settings)

    pub fn home(&self) -> &'static str {
        self.t("홈", "Home", "ホーム", "Inicio")
    }
    pub fn collection(&self) -> &'static str {
        self.t("컬렉션", "Collection", "コレクション", "Colección")
    }
    pub fn settings(&self) -> &'static str {
        self.t("설정", "Settings", "設定", "Ajustes")
    }
    pub fn back(&self) -> &'static str {
        self.t("뒤로", "Back", "戻る", "Atrás")
    }
    pub fn quit(&self) -> &'static str {
        self.t("종료", "Quit", "終了", "Salir")
    }
    pub fn language(&self) -> &'static str {
        self.t("언어", "Language", "言語", "Idioma")
    }
    pub fn week_starts(&self) -> &'static str {
        self.t(
            "주간 시작 요일",
            "Week starts on",
            "週の開始日は",
            "La semana empieza el",
        )
    }
    pub fn monday(&self) -> &'static str {
        self.t("월요일", "Monday", "月曜日", "Lunes")
    }
    pub fn sunday(&self) -> &'static str {
        self.t("일요일", "Sunday", "日曜日", "Domingo")
    }

    // MARK: settings — launch at login, floating pet, save transfer

    pub fn launch_at_login(&self) -> &'static str {
        self.t(
            "로그인 시 실행",
            "Launch at login",
            "ログイン時に起動",
            "Iniciar al iniciar sesión",
        )
    }
    pub fn launch_at_login_hint(&self) -> &'static str {
        self.t(
            "로그인할 때 PokeTokenBar를 시작해요 (XDG autostart).",
            "Starts PokeTokenBar when you sign in (XDG autostart).",
            "サインイン時に PokeTokenBar を起動します（XDG autostart）。",
            "Inicia PokeTokenBar al iniciar sesión (XDG autostart).",
        )
    }
    pub fn floating_pet(&self) -> &'static str {
        self.t(
            "떠 있는 펫",
            "Floating pet",
            "フローティングペット",
            "Mascota flotante",
        )
    }
    pub fn floating_pet_hint(&self) -> &'static str {
        self.t(
            "컴패니언이 데스크톱 위에 떠 있어요. 클릭하면 앱이 열리고, 우클릭하면 메뉴가 나와요.",
            "Your companion lives on the desktop. Click to open the app; right-click for the menu.",
            "パートナーがデスクトップ上に常駐します。クリックでアプリを開き、右クリックでメニュー。",
            "Tu compañero vive en el escritorio. Clic para abrir la app; clic derecho para el menú.",
        )
    }
    pub fn pet_size(&self) -> &'static str {
        self.t(
            "펫 크기",
            "Pet size",
            "ペットのサイズ",
            "Tamaño de la mascota",
        )
    }
    pub fn save_data(&self) -> &'static str {
        self.t(
            "세이브 데이터",
            "Save data",
            "セーブデータ",
            "Datos de guardado",
        )
    }
    pub fn save_hint(&self) -> &'static str {
        self.t(
            "기기 교체용 세이브를 내보내고 다른 기기에서 가져와요. 가져오기 전에는 현재 상태를 백업해요.",
            "Export a save to move to another device and import it there. Importing backs up the current state first.",
            "機種変更用のセーブを出力し、別の機種で読み込みます。読み込む前に現在の状態をバックアップします。",
            "Exporta un guardado para cambiar de dispositivo e impórtalo allí. La importación respalda el estado actual primero.",
        )
    }
    pub fn export_save(&self) -> &'static str {
        self.t(
            "내보내기…",
            "Export save…",
            "セーブを出力…",
            "Exportar guardado…",
        )
    }
    pub fn import_save(&self) -> &'static str {
        self.t(
            "가져오기…",
            "Import save…",
            "セーブを読み込み…",
            "Importar guardado…",
        )
    }
    pub fn import_confirm(&self, dex: i64, tokens: i64) -> String {
        let s = self.t(
            "현재 진행을 도감 {dex}종·평생 토큰 {tokens}의 세이브로 교체할까요?",
            "Replace the current progress with a save of {dex} dex species and {tokens} lifetime tokens?",
            "現在の進行を（図鑑 {dex}種・生涯トークン {tokens}）のセーブで置き換えますか？",
            "¿Reemplazar el progreso actual con un guardado de {dex} especies de Pokédex y {tokens} tokens de por vida?",
        );
        s.replace("{dex}", &dex.to_string())
            .replace("{tokens}", &tokens.to_string())
    }
    pub fn replace(&self) -> &'static str {
        self.t("교체", "Replace", "置き換える", "Reemplazar")
    }
    pub fn import_not_save(&self) -> &'static str {
        self.t(
            "그 파일은 PokeTokenBar 세이브가 아니에요.",
            "That file is not a PokeTokenBar save.",
            "そのファイルは PokeTokenBar のセーブではありません。",
            "Ese archivo no es un guardado de PokeTokenBar.",
        )
    }
    pub fn import_newer(&self, found: u32) -> String {
        let s = self.t(
            "그 세이브는 더 새 버전의 세이브예요 (스키마 {found}). 앱을 업데이트하세요.",
            "That save is from a newer version (schema {found}). Update the app first.",
            "そのセーブはより新しいバージョンのものです（スキマ {found}）。先にアプリを更新してください。",
            "Ese guardado es de una versión más nueva (esquema {found}). Actualiza la app primero.",
        );
        s.replace("{found}", &found.to_string())
    }
    pub fn import_too_large(&self) -> &'static str {
        self.t(
            "그 파일은 세이브로 보기엔 너무 커요 (8 MB 한도).",
            "That file is too large to be a save (8 MB limit).",
            "そのファイルはセーブとしては大きすぎます（8 MB 上限）。",
            "Ese archivo es demasiado grande para ser un guardado (límite de 8 MB).",
        )
    }
    pub fn import_backup_failed(&self) -> &'static str {
        self.t(
            "이전 상태를 백업할 수 없어서 가져오기를 취소했어요.",
            "The import was cancelled: the current state could not be backed up first.",
            "読み込みをキャンセルしました：現在の状態を先にバックアップできませんでした。",
            "Se canceló la importación: no se pudo respaldar el estado actual antes.",
        )
    }
    pub fn import_failed(&self) -> &'static str {
        self.t(
            "세이브를 가져올 수 없어요 (파일 오류).",
            "The save could not be imported (file error).",
            "セーブを読み込めませんでした（ファイルエラー）。",
            "No se pudo importar el guardado (error de archivo).",
        )
    }
    pub fn save_exported(&self) -> &'static str {
        self.t(
            "세이브를 내보냈어요.",
            "Save exported.",
            "セーブを出力しました。",
            "Guardado exportado.",
        )
    }
    pub fn save_imported(&self) -> &'static str {
        self.t(
            "세이브를 가져왔어요. 이전 상태는 백업했어요.",
            "Save imported. The previous state was backed up.",
            "セーブを読み込みました。前の状態はバックアップ済みです。",
            "Guardado importado. El estado anterior se respaldó.",
        )
    }
    pub fn floating_pet_menu_open(&self) -> &'static str {
        self.t("열기", "Open", "開く", "Abrir")
    }
    pub fn floating_pet_menu_hide(&self) -> &'static str {
        self.t(
            "펫 숨기기",
            "Hide floating pet",
            "ペットを非表示",
            "Ocultar mascota flotante",
        )
    }
    pub fn error_heading(&self) -> &'static str {
        self.t("오류", "Error", "エラー", "Error")
    }
    pub fn ok(&self) -> &'static str {
        self.t("확인", "OK", "OK", "OK")
    }
    pub fn today(&self) -> &'static str {
        self.t("오늘", "Today", "今日", "Hoy")
    }
    pub fn week(&self) -> &'static str {
        self.t("이번 주", "Week", "今週", "Semana")
    }
    pub fn month(&self) -> &'static str {
        self.t("이번 달", "Month", "今月", "Mes")
    }
    pub fn burn(&self) -> &'static str {
        self.t("소모", "Burn", "燃焼", "Consumo")
    }
    pub fn limits_official(&self) -> &'static str {
        self.t(
            "한도 (공식)",
            "Limits (official)",
            "上限（公式）",
            "Límites (oficial)",
        )
    }
    pub fn five_hour_session(&self) -> &'static str {
        self.t(
            "5시간 세션",
            "5-hour session",
            "5時間セッション",
            "Sesión de 5 horas",
        )
    }
    pub fn weekly(&self) -> &'static str {
        self.t("주간", "Weekly", "週間", "Semanal")
    }
    pub fn plan(&self, p: &str) -> String {
        let s = self.t("플랜 {p}", "Plan {p}", "プラン {p}", "Plan {p}");
        s.replace("{p}", p)
    }
    pub fn limits_unavailable(&self) -> &'static str {
        self.t(
            "한도 정보를 불러오지 못해요",
            "Not available",
            "利用不可",
            "No disponible",
        )
    }
    pub fn limit_reached(&self) -> &'static str {
        self.t("한도 도달", "Limit reached", "上限到達", "Límite alcanzado")
    }
    pub fn provider_active(&self) -> &'static str {
        self.t("활성", "Active", "アクティブ", "Activo")
    }
    pub fn provider_idle(&self) -> &'static str {
        self.t("가만", "Idle", "アイドル", "Inactivo")
    }
    pub fn tok_in(&self) -> &'static str {
        self.t("in", "in", "in", "in")
    }
    pub fn tok_out(&self) -> &'static str {
        self.t("out", "out", "out", "out")
    }
    pub fn tok_cache_write(&self) -> &'static str {
        self.t("cache w", "cache w", "cache w", "cache w")
    }
    pub fn tok_cache_read(&self) -> &'static str {
        self.t("cache r", "cache r", "cache r", "cache r")
    }
    pub fn dex_page_prev(&self) -> &'static str {
        self.t(
            "이전 페이지",
            "Previous page",
            "前のページ",
            "Página anterior",
        )
    }
    pub fn dex_page_next(&self) -> &'static str {
        self.t("다음 페이지", "Next page", "次のページ", "Página siguiente")
    }
}

/// Compact token amount (port of `TokenFormatter.compact`): `999`, `1.2K`, `100M`, `1.50B`…
/// trailing zeros are trimmed.
pub fn compact_tokens(value: i64) -> String {
    let v = (value.abs()) as f64;
    let sign = if value < 0 { "-" } else { "" };
    let body = if v < 1_000.0 {
        value.abs().to_string()
    } else if v < 1_000_000.0 {
        trim(value as f64 / 1_000.0, 1) + "K"
    } else if v < 1_000_000_000.0 {
        trim(value as f64 / 1_000_000.0, 1) + "M"
    } else {
        trim(value as f64 / 1_000_000_000.0, 2) + "B"
    };
    format!("{sign}{body}")
}

fn trim(value: f64, decimals: usize) -> String {
    let mut s = format!("{:.*}", decimals, value);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_and_labels() {
        assert_eq!(Language::from_code("en"), Some(Language::En));
        assert_eq!(Language::from_code("JA"), Some(Language::Ja));
        assert_eq!(Language::from_code("fr"), None);
        assert_eq!(Language::En.label(), "English");
        assert_eq!(Language::Ko.label(), "한국어");
        assert_eq!(Language::Ja.label(), "日本語");
        assert_eq!(Language::Es.label(), "Español");
    }

    #[test]
    fn resolve_prefers_env_then_state_then_config() {
        // env override wins (set within this test process; other tests keep their own)
        std::env::set_var("PTB_LANG", "ko");
        assert_eq!(resolve_language("ja", "es"), Language::Ko);
        std::env::remove_var("PTB_LANG");
        assert_eq!(resolve_language("ja", "es"), Language::Ja);
        assert_eq!(resolve_language("bogus", "es"), Language::Es);
        assert_eq!(resolve_language("", ""), Language::En);
    }

    #[test]
    fn status_strings_per_language() {
        assert_eq!(L::new(Language::En).status_focus(), "In focus mode now.");
        assert_eq!(L::new(Language::Ko).status_focus(), "지금은 집중 모드예요.");
        assert_eq!(L::new(Language::Ja).status_focus(), "今は集中モードです。");
        assert_eq!(
            L::new(Language::Es).status_focus(),
            "Ahora está en modo concentración."
        );
        assert_eq!(
            L::new(Language::En).status_evolved("Charizard"),
            "Evolved into Charizard!"
        );
        assert_eq!(
            L::new(Language::Ko).status_evolved("리ザ드온"),
            "리ザ드온(으)로 진화했어요!"
        );
    }

    #[test]
    fn rarity_and_egg_names() {
        let en = L::new(Language::En);
        assert_eq!(en.rarity_label(Rarity::Rare), "Rare");
        assert_eq!(en.egg_name(None), "Pokémon Egg");
        assert_eq!(en.egg_name(Some(Rarity::Rare)), "Rare Egg");
        let ja = L::new(Language::Ja);
        assert_eq!(ja.egg_name(Some(Rarity::Uncommon)), "アンコモンのタマゴ");
        let ko = L::new(Language::Ko);
        assert_eq!(ko.rarity_label(Rarity::Legendary), "전설");
    }

    #[test]
    fn item_names_and_candy_description_uses_constant() {
        let en = L::new(Language::En);
        assert_eq!(en.item_name(ItemKind::RareCandy), "Rare Candy");
        assert_eq!(en.item_name(ItemKind::Mint), "Mint");
        assert_eq!(en.item_name(ItemKind::ShinyCharm), "Shiny Charm");
        // 100M XP renders as "100M" (derived from the constant, not hard-coded).
        assert!(
            en.item_description(ItemKind::RareCandy).contains("100M"),
            "{}",
            en.item_description(ItemKind::RareCandy)
        );
    }

    #[test]
    fn chrome_strings_present_in_all_languages() {
        let en = L::new(Language::En);
        assert_eq!(en.home(), "Home");
        assert_eq!(en.collection(), "Collection");
        assert_eq!(en.limits_unavailable(), "Not available");
        assert_eq!(en.plan("Max 20x"), "Plan Max 20x");
        for lang in Language::ALL {
            let l = L::new(lang);
            for s in [
                l.home(),
                l.collection(),
                l.settings(),
                l.back(),
                l.quit(),
                l.language(),
                l.week_starts(),
                l.monday(),
                l.sunday(),
                l.today(),
                l.week(),
                l.month(),
                l.burn(),
                l.limits_official(),
                l.five_hour_session(),
                l.weekly(),
                l.limits_unavailable(),
                l.limit_reached(),
                l.provider_active(),
                l.provider_idle(),
                l.dex_page_prev(),
                l.dex_page_next(),
            ] {
                assert!(!s.is_empty(), "{lang:?}: empty chrome string");
            }
        }
    }

    #[test]
    fn compact_token_formats() {
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(1_234), "1.2K");
        assert_eq!(compact_tokens(100_000_000), "100M");
        assert_eq!(compact_tokens(1_500_000_000), "1.5B");
        assert_eq!(compact_tokens(2_000_000_000), "2B");
        assert_eq!(compact_tokens(-500), "-500");
    }
}
