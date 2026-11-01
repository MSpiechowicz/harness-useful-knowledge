//! The calibration queries (X6): what a person or an agent asks the family's documentation, with
//! the documents that hold the answer. A document is named by the end of its source key in the
//! calibration corpus (`desktop/docs/hooks-format.md`), and a query is answered when any of its
//! documents is among the first three results.

/// One query and where its answer is.
pub struct Case {
    pub query: &'static str,
    /// `en` or `pl`: the documentation is English, so a Polish query only has the vectors.
    pub lang: &'static str,
    pub expect: &'static [&'static str],
}

const fn en(query: &'static str, expect: &'static [&'static str]) -> Case {
    Case {
        query,
        lang: "en",
        expect,
    }
}

const fn pl(query: &'static str, expect: &'static [&'static str]) -> Case {
    Case {
        query,
        lang: "pl",
        expect,
    }
}

pub const CASES: &[Case] = &[
    en(
        "how does a release get published after CI passes",
        &["desktop/docs/release-and-updater-keys.md"],
    ),
    en(
        "what happens if the updater signing private key is lost",
        &["desktop/docs/release-and-updater-keys.md"],
    ),
    en(
        "cron expression fields and step syntax",
        &["desktop/docs/routines-format.md"],
    ),
    en(
        "what happens to a routine scheduled inside the daylight saving gap",
        &["desktop/docs/routines-format.md"],
    ),
    en(
        "which hook events allow a followUp action",
        &["desktop/docs/hooks-format.md"],
    ),
    en("matcher grammar for toolEnd hooks", &["desktop/docs/hooks-format.md"]),
    en(
        "how to add a new harness adapter and its capabilities",
        &["desktop/docs/adding-a-harness.md"],
    ),
    en(
        "HarnessId enum step when adding a harness",
        &["desktop/docs/adding-a-harness.md"],
    ),
    en(
        "what does a hud1 share setup string contain",
        &["desktop/docs/share-setup.md"],
    ),
    en(
        "what is never exported when sharing a setup",
        &["desktop/docs/share-setup.md"],
    ),
    en("threat model of the desktop app", &["desktop/docs/security-privacy.md"]),
    en(
        "which files does the app never read",
        &["desktop/docs/security-privacy.md"],
    ),
    en(
        "how are secrets redacted before they are logged",
        &["desktop/docs/security-privacy.md"],
    ),
    en(
        "request flow from the webview command to the adapter",
        &["desktop/docs/architecture.md"],
    ),
    en(
        "IPC contract between the frontend and the Rust commands",
        &["desktop/docs/architecture.md"],
    ),
    en(
        "script approval and trust in project memory",
        &[
            "memory/README.md",
            "desktop/docs/security-privacy.md",
            "desktop/docs/architecture.md",
        ],
    ),
    en("how to register harness memory with Claude Code", &["memory/README.md"]),
    en("install the harness skills for OpenCode", &["skills/README.md"]),
    en("how are model roles configured for Codex", &["skills/README.md"]),
    en("retired Graphify integration", &["skills/README.md"]),
    en(
        "code graph symbol resolution order from the prototype",
        &["memory/docs/plans/code-graph.md"],
    ),
    en(
        "worker process for the code index added after the fourth review round",
        &["memory/docs/plans/code-graph.md"],
    ),
    en(
        "save guards gate order and held items",
        &[
            "desktop/docs/plans/memory-save-guards.md",
            "memory/README.md",
            "desktop/docs/security-privacy.md",
        ],
    ),
    en(
        "why the OpenCode chat uses the native server API instead of ACP",
        &[
            "desktop/docs/plans/opencode-native-chat.md",
            "desktop/docs/plans/opencode-support.md",
        ],
    ),
    en(
        "reciprocal rank fusion and spike thresholds for hybrid search",
        &[
            "desktop/docs/plans/hybrid-search.md",
            "memory/docs/plans/hybrid-search.md",
            "memory/README.md",
        ],
    ),
    pl(
        "jak działają godziny letnie w zaplanowanych rutynach",
        &["desktop/docs/routines-format.md"],
    ),
    pl(
        "co się stanie gdy klucz prywatny do podpisywania aktualizacji zostanie utracony",
        &["desktop/docs/release-and-updater-keys.md"],
    ),
    pl(
        "jakie zdarzenia obsługują akcję followUp w hookach",
        &["desktop/docs/hooks-format.md"],
    ),
    pl(
        "jak zarejestrować pamięć projektu w narzędziu Claude Code",
        &["memory/README.md"],
    ),
    pl("instalacja umiejętności dla Codex", &["skills/README.md"]),
    pl(
        "model zagrożeń aplikacji desktopowej",
        &["desktop/docs/security-privacy.md"],
    ),
    pl(
        "których plików aplikacja nigdy nie czyta",
        &["desktop/docs/security-privacy.md"],
    ),
    pl(
        "jak dodać nowego agenta kodującego do aplikacji",
        &["desktop/docs/adding-a-harness.md"],
    ),
    pl(
        "ukrywanie sekretów w komunikatach i logach",
        &["desktop/docs/security-privacy.md"],
    ),
    pl(
        "jak powstaje wydanie po przejściu testów CI",
        &["desktop/docs/release-and-updater-keys.md"],
    ),
    pl(
        "format pliku udostępnionej konfiguracji",
        &["desktop/docs/share-setup.md"],
    ),
    pl(
        "graf kodu i kolejność rozwiązywania symboli",
        &["memory/docs/plans/code-graph.md"],
    ),
    pl(
        "dlaczego czat OpenCode używa natywnego API serwera zamiast ACP",
        &[
            "desktop/docs/plans/opencode-native-chat.md",
            "desktop/docs/plans/opencode-support.md",
        ],
    ),
    pl(
        "hooki zespołowe wspólne dla całego zespołu",
        &["desktop/docs/hooks-format.md"],
    ),
    pl("harmonogram cron w rutynach", &["desktop/docs/routines-format.md"]),
];

/// Queries the documentation has no answer to: a search must find nothing for them (the recall
/// hook above all).
pub const NO_ANSWER: &[&str] = &[
    "best recipe for chocolate chip cookies",
    "football world cup final result",
    "capital city of Australia",
    "przepis na pierogi ruskie",
    "kurs wymiany dolara do złotego",
    "kto wygrał mistrzostwa świata w piłce nożnej",
];
