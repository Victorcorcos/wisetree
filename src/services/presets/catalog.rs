//! Static catalog of language/framework setup presets.
//!
//! Each [`Preset`] bundles the three knobs the user wants pre-filled when
//! bootstrapping a new project: `worktreeCopyPatterns`, `worktreeCopyIgnores`,
//! and `postCreateCmd`. Detection signatures live alongside each entry so the
//! menu can pre-select the right preset.

use std::path::Path;

use crate::services::presets::detect::Signature;

/// Stable identifier used to look up a preset across runs (e.g. in
/// `.wisetree.json`, in TUI tests, and in the auto-detect fallback chain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PresetId {
    RubyOnRails,
    Django,
    Flask,
    FastApi,
    NextJs,
    React,
    VueNuxt,
    Angular,
    Svelte,
    Astro,
    Remix,
    ExpressNode,
    NestJs,
    Flutter,
    SpringBootMaven,
    SpringBootGradle,
    DotNet,
    Go,
    Rust,
    Laravel,
    Phoenix,
    Android,
    Ios,
    Generic,
}

impl PresetId {
    /// String form used for serialization and lookups.
    pub fn as_str(self) -> &'static str {
        match self {
            PresetId::RubyOnRails => "ruby_on_rails",
            PresetId::Django => "django",
            PresetId::Flask => "flask",
            PresetId::FastApi => "fastapi",
            PresetId::NextJs => "nextjs",
            PresetId::React => "react",
            PresetId::VueNuxt => "vue_nuxt",
            PresetId::Angular => "angular",
            PresetId::Svelte => "svelte",
            PresetId::Astro => "astro",
            PresetId::Remix => "remix",
            PresetId::ExpressNode => "express_node",
            PresetId::NestJs => "nestjs",
            PresetId::Flutter => "flutter",
            PresetId::SpringBootMaven => "spring_boot_maven",
            PresetId::SpringBootGradle => "spring_boot_gradle",
            PresetId::DotNet => "dotnet",
            PresetId::Go => "go",
            PresetId::Rust => "rust",
            PresetId::Laravel => "laravel",
            PresetId::Phoenix => "phoenix",
            PresetId::Android => "android",
            PresetId::Ios => "ios",
            PresetId::Generic => "generic",
        }
    }
}

/// A preset bundles UI metadata, detection rules, and the three lists that
/// will be written into `.wisetree.json` when the user confirms.
#[derive(Debug, Clone)]
pub struct Preset {
    pub id: PresetId,
    pub label: &'static str,
    pub description: &'static str,
    pub copy_patterns: Vec<&'static str>,
    pub copy_ignores: Vec<&'static str>,
    pub post_create_cmd: Vec<&'static str>,
    pub signature: Signature,
}

impl Preset {
    /// True when this preset's signature matches the given project root.
    pub fn matches(&self, root: &Path) -> bool {
        self.signature.matches(root)
    }

    pub fn copy_patterns_owned(&self) -> Vec<String> {
        self.copy_patterns.iter().map(|s| s.to_string()).collect()
    }

    pub fn copy_ignores_owned(&self) -> Vec<String> {
        self.copy_ignores.iter().map(|s| s.to_string()).collect()
    }

    pub fn post_create_cmd_owned(&self) -> Vec<String> {
        self.post_create_cmd.iter().map(|s| s.to_string()).collect()
    }
}

/// Returns the full preset catalog in the order they are listed in `PLAN.md`.
/// Catalog order is also the tie-breaker for `detect()`: the first matching
/// signature wins, so more specific entries (Next.js) appear before less
/// specific ones (Express / generic Node).
pub fn catalog() -> Vec<Preset> {
    vec![
        Preset {
            id: PresetId::RubyOnRails,
            label: "Ruby on Rails",
            description: "Rails app — Bundler + ActiveRecord",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                "config/master.key",
                "config/credentials/*.key",
                "config/database.yml",
                "config/application.yml",
                "config/secrets.yml",
                ".rbenv-vars",
            ],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/tmp/**",
                "**/log/**",
                "**/storage/**",
                "**/vendor/bundle/**",
                "**/.bundle/**",
                "**/public/assets/**",
                "**/public/packs/**",
                "**/coverage/**",
                "**/.git/**",
                "**/.DS_Store",
            ],
            post_create_cmd: vec![
                "bundle install --jobs 5 --verbose --retry 4",
                "yarn install",
                "bin/rails db:prepare",
            ],
            signature: Signature::all_of(&[
                Signature::file_exists("Gemfile"),
                Signature::any_of(&[
                    Signature::file_exists("config/application.rb"),
                    Signature::file_exists("bin/rails"),
                ]),
            ]),
        },
        Preset {
            id: PresetId::Django,
            label: "Django",
            description: "Python web framework with manage.py",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                "**/local_settings.py",
                "**/settings_local.py",
                "**/secrets.json",
                "db.sqlite3",
            ],
            copy_ignores: vec![
                "**/__pycache__/**",
                "**/.venv/**",
                "**/venv/**",
                "**/*.pyc",
                "**/staticfiles/**",
                "**/media/**",
                "**/.pytest_cache/**",
                "**/.mypy_cache/**",
                "**/.git/**",
            ],
            post_create_cmd: vec![
                "python -m venv .venv",
                ".venv/bin/pip install -r requirements.txt",
                ".venv/bin/python manage.py migrate",
            ],
            signature: Signature::all_of(&[
                Signature::file_exists("manage.py"),
                Signature::any_of(&[
                    Signature::file_glob("requirements*.txt"),
                    Signature::file_exists("pyproject.toml"),
                ]),
            ]),
        },
        Preset {
            id: PresetId::FastApi,
            label: "FastAPI",
            description: "Python async API framework",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                "**/secrets/**",
                "**/*.pem",
            ],
            copy_ignores: vec![
                "**/__pycache__/**",
                "**/.venv/**",
                "**/venv/**",
                "**/*.pyc",
                "**/.pytest_cache/**",
                "**/.mypy_cache/**",
                "**/.ruff_cache/**",
                "**/htmlcov/**",
                "**/.git/**",
            ],
            post_create_cmd: vec![
                "python -m venv .venv",
                ".venv/bin/pip install -r requirements.txt",
                ".venv/bin/alembic upgrade head || true",
            ],
            signature: Signature::any_of(&[
                Signature::file_contains("pyproject.toml", "fastapi"),
                Signature::glob_contains("requirements*.txt", "fastapi"),
            ]),
        },
        Preset {
            id: PresetId::Flask,
            label: "Flask",
            description: "Python micro web framework",
            copy_patterns: vec![
                "**/.env*",
                ".flaskenv",
                ".vscode/**",
                ".idea/**",
                "**/instance/**",
                "**/config_local.py",
            ],
            copy_ignores: vec![
                "**/__pycache__/**",
                "**/.venv/**",
                "**/venv/**",
                "**/*.pyc",
                "**/.pytest_cache/**",
                "**/.mypy_cache/**",
                "**/.git/**",
            ],
            post_create_cmd: vec![
                "python -m venv .venv",
                ".venv/bin/pip install -r requirements.txt",
            ],
            signature: Signature::any_of(&[
                Signature::glob_contains("requirements*.txt", "Flask"),
                Signature::file_contains("app.py", "from flask"),
                Signature::file_contains("wsgi.py", "from flask"),
            ]),
        },
        Preset {
            id: PresetId::NextJs,
            label: "Next.js",
            description: "React meta-framework (App Router / Pages)",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**", ".vercel/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/.next/**",
                "**/out/**",
                "**/dist/**",
                "**/.turbo/**",
                "**/.swc/**",
                "**/coverage/**",
                "**/.eslintcache",
                "**/.DS_Store",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::any_of(&[
                Signature::file_exists("next.config.js"),
                Signature::file_exists("next.config.mjs"),
                Signature::file_exists("next.config.ts"),
                Signature::file_contains("package.json", "\"next\""),
            ]),
        },
        Preset {
            id: PresetId::Remix,
            label: "Remix",
            description: "Full-stack React framework",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                ".vercel/**",
                ".wrangler/**",
            ],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/out/**",
                "**/dist/**",
                "**/build/**",
                "**/.cache/**",
                "**/coverage/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::any_of(&[
                Signature::file_exists("remix.config.js"),
                Signature::file_exists("remix.config.mjs"),
                Signature::file_exists("remix.config.ts"),
            ]),
        },
        Preset {
            id: PresetId::NestJs,
            label: "NestJS",
            description: "Progressive Node.js framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/dist/**",
                "**/build/**",
                "**/coverage/**",
                "**/.nyc_output/**",
                "**/logs/**",
                "**/*.log",
                "**/*.tsbuildinfo",
                "**/.eslintcache",
                "**/pids/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::file_exists("nest-cli.json"),
        },
        Preset {
            id: PresetId::VueNuxt,
            label: "Vue / Nuxt",
            description: "Vue.js + Nuxt meta-framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/.nuxt/**",
                "**/.output/**",
                "**/.nitro/**",
                "**/.data/**",
                "**/dist/**",
                "**/build/**",
                "**/coverage/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::any_of(&[
                Signature::file_exists("nuxt.config.js"),
                Signature::file_exists("nuxt.config.ts"),
                Signature::file_contains("package.json", "\"nuxt\""),
                Signature::file_contains("package.json", "\"vue\""),
            ]),
        },
        Preset {
            id: PresetId::Angular,
            label: "Angular",
            description: "Google's TypeScript web framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/.angular/**",
                "**/dist/**",
                "**/tmp/**",
                "**/out-tsc/**",
                "**/.nx/**",
                "**/coverage/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::file_exists("angular.json"),
        },
        Preset {
            id: PresetId::Svelte,
            label: "Svelte / SvelteKit",
            description: "Reactive component framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**", ".vercel/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/.svelte-kit/**",
                "**/.vite/**",
                "**/.netlify/**",
                "**/dist/**",
                "**/build/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::any_of(&[
                Signature::file_exists("svelte.config.js"),
                Signature::file_exists("svelte.config.ts"),
            ]),
        },
        Preset {
            id: PresetId::Astro,
            label: "Astro",
            description: "Static-first multi-framework site builder",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                ".vercel/**",
                ".netlify/**",
            ],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/.astro/**",
                "**/.vercel/**",
                "**/.netlify/**",
                "**/dist/**",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::any_of(&[
                Signature::file_exists("astro.config.mjs"),
                Signature::file_exists("astro.config.ts"),
                Signature::file_exists("astro.config.js"),
            ]),
        },
        Preset {
            id: PresetId::React,
            label: "React (CRA / Vite)",
            description: "Plain React app (Create React App, Vite, etc.)",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/build/**",
                "**/dist/**",
                "**/.vite/**",
                "**/.parcel-cache/**",
                "**/coverage/**",
                "**/storybook-static/**",
                "**/.eslintcache",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::file_contains("package.json", "\"react\""),
        },
        Preset {
            id: PresetId::ExpressNode,
            label: "Express / Node.js",
            description: "Plain Node.js project",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/dist/**",
                "**/build/**",
                "**/coverage/**",
                "**/.nyc_output/**",
                "**/.npm/**",
                "**/logs/**",
                "**/*.log",
                "**/*.tsbuildinfo",
                "**/pids/**",
                "**/*.pid",
                "**/*.seed",
            ],
            post_create_cmd: vec!["npm install"],
            signature: Signature::file_exists("package.json"),
        },
        Preset {
            id: PresetId::Flutter,
            label: "Flutter / Dart",
            description: "Cross-platform Dart UI toolkit",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                "android/local.properties",
                "android/key.properties",
                "android/app/google-services.json",
                "android/app/upload-keystore.jks",
                "ios/Runner/GoogleService-Info.plist",
                "ios/Flutter/Debug.xcconfig",
                "**/.fvm/fvm_config.json",
            ],
            copy_ignores: vec![
                "**/.dart_tool/**",
                "**/build/**",
                "**/.pub-cache/**",
                "**/.flutter-plugins*",
                "**/.idea/**",
                "**/*.iml",
                "**/ios/.symlinks/**",
                "**/ios/Flutter/Generated.xcconfig",
                "**/ios/Pods/**",
            ],
            post_create_cmd: vec![
                "flutter pub get",
                "(cd ios && pod install --silent) || true",
            ],
            signature: Signature::all_of(&[
                Signature::file_exists("pubspec.yaml"),
                Signature::any_of(&[
                    Signature::file_exists("lib/main.dart"),
                    Signature::file_exists("analysis_options.yaml"),
                ]),
            ]),
        },
        Preset {
            id: PresetId::SpringBootMaven,
            label: "Spring Boot (Maven)",
            description: "Java/Kotlin Spring Boot via Maven",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/runConfigurations/**",
                ".mvn/maven.config",
                ".mvn/jvm.config",
                "**/src/main/resources/application-local.yml",
                "**/src/main/resources/application-local.yaml",
                "**/src/main/resources/application-local.properties",
            ],
            copy_ignores: vec![
                "**/target/**",
                "**/.gradle/**",
                "**/build/**",
                "**/out/**",
                "**/.idea/**",
                "**/*.iml",
                "**/.settings/**",
                "**/.project",
                "**/.classpath",
            ],
            post_create_cmd: vec!["./mvnw -DskipTests package"],
            signature: Signature::file_contains("pom.xml", "spring-boot"),
        },
        Preset {
            id: PresetId::SpringBootGradle,
            label: "Spring Boot (Gradle)",
            description: "Java/Kotlin Spring Boot via Gradle",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/runConfigurations/**",
                "gradle.properties",
                "**/gradle-local.properties",
                "**/src/main/resources/application-local.yml",
                "**/src/main/resources/application-local.yaml",
                "**/src/main/resources/application-local.properties",
            ],
            copy_ignores: vec![
                "**/build/**",
                "**/.gradle/**",
                "**/out/**",
                "**/target/**",
                "**/.idea/**",
                "**/*.iml",
                "**/.settings/**",
                "**/.project",
                "**/.classpath",
            ],
            post_create_cmd: vec!["./gradlew build -x test"],
            signature: Signature::any_of(&[
                Signature::file_contains("build.gradle", "spring-boot"),
                Signature::file_contains("build.gradle.kts", "spring-boot"),
            ]),
        },
        Preset {
            id: PresetId::Android,
            label: "Android (Gradle)",
            description: "Native Android app project",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                "local.properties",
                "keystore.properties",
                "signing.properties",
                "*.keystore",
                "*.jks",
                "google-services.json",
                "app/google-services.json",
                ".idea/runConfigurations/**",
            ],
            copy_ignores: vec![
                "**/build/**",
                "**/.gradle/**",
                "**/.idea/**",
                "**/*.iml",
                "**/captures/**",
                "**/release/**",
                "**/.cxx/**",
            ],
            post_create_cmd: vec!["./gradlew assembleDebug"],
            signature: Signature::all_of(&[
                Signature::any_of(&[
                    Signature::file_exists("settings.gradle"),
                    Signature::file_exists("settings.gradle.kts"),
                ]),
                Signature::any_of(&[
                    Signature::file_exists("app/build.gradle"),
                    Signature::file_exists("app/build.gradle.kts"),
                ]),
                Signature::file_exists("app/src/main/AndroidManifest.xml"),
            ]),
        },
        Preset {
            id: PresetId::Ios,
            label: "iOS / Xcode (Swift)",
            description: "Apple platform app",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                "**/*.xcconfig",
                "**/GoogleService-Info.plist",
                "**/fastlane/.env*",
                "**/*.p8",
                "**/*.p12",
                "**/*.mobileprovision",
            ],
            copy_ignores: vec![
                "**/Pods/**",
                "**/build/**",
                "**/DerivedData/**",
                "**/xcuserdata/**",
                "**/.swiftpm/**",
                "**/Carthage/Build/**",
                "**/Carthage/Checkouts/**",
                "**/fastlane/report.xml",
                "**/fastlane/Preview.html",
                "**/fastlane/screenshots/**",
                "**/fastlane/test_output/**",
            ],
            post_create_cmd: vec![
                "pod install --silent || true",
                "xcodebuild -resolvePackageDependencies -quiet || true",
            ],
            signature: Signature::any_of(&[
                Signature::file_glob("*.xcodeproj"),
                Signature::file_glob("*.xcworkspace"),
            ]),
        },
        Preset {
            id: PresetId::DotNet,
            label: ".NET / ASP.NET Core",
            description: "C# / .NET project",
            copy_patterns: vec![
                "**/.env*",
                ".vscode/**",
                ".idea/**",
                "**/appsettings.Development.json",
                "**/appsettings.Local.json",
                "**/appsettings.*.json",
                "**/secrets.json",
                "**/Properties/launchSettings.json",
                "**/*.user",
                "**/PublishProfiles/**",
            ],
            copy_ignores: vec![
                "**/bin/**",
                "**/obj/**",
                "**/.vs/**",
                "**/*.suo",
                "**/TestResults/**",
                "**/artifacts/**",
                "**/coverage/**",
            ],
            post_create_cmd: vec![
                "dotnet restore",
                "dotnet dev-certs https --check --trust >/dev/null 2>&1 || true",
            ],
            signature: Signature::any_of(&[
                Signature::file_glob("*.csproj"),
                Signature::file_glob("*.sln"),
            ]),
        },
        Preset {
            id: PresetId::Go,
            label: "Go",
            description: "Go modules project",
            copy_patterns: vec![
                "**/.env*",
                "**/.envrc",
                ".vscode/**",
                ".idea/**",
                "air.toml",
                ".dlv-config",
            ],
            copy_ignores: vec![
                "**/vendor/**",
                "**/bin/**",
                "**/*.exe",
                "**/*.test",
                "**/*.out",
                "**/*.cover",
                "**/*.coverprofile",
            ],
            post_create_cmd: vec!["go mod download", "go work sync || true"],
            signature: Signature::file_exists("go.mod"),
        },
        Preset {
            id: PresetId::Rust,
            label: "Rust / Cargo",
            description: "Cargo workspace or crate",
            copy_patterns: vec!["**/.env*", "**/.envrc", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/target/**",
                "**/*.rs.bk",
                "**/*.pdb",
                "**/flamegraph.svg",
                "**/perf.data",
                "**/perf.data.old",
            ],
            post_create_cmd: vec!["cargo fetch"],
            signature: Signature::file_exists("Cargo.toml"),
        },
        Preset {
            id: PresetId::Laravel,
            label: "Laravel (PHP)",
            description: "PHP framework with Artisan",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**", "auth.json"],
            copy_ignores: vec![
                "**/vendor/**",
                "**/node_modules/**",
                "**/storage/framework/**",
                "**/storage/logs/**",
                "**/bootstrap/cache/**",
                "**/public/build/**",
                "**/public/hot",
                "**/coverage/**",
            ],
            post_create_cmd: vec![
                "composer install",
                "php artisan key:generate",
                "php artisan migrate",
            ],
            signature: Signature::any_of(&[
                Signature::file_contains("composer.json", "laravel/framework"),
                Signature::file_exists("artisan"),
            ]),
        },
        Preset {
            id: PresetId::Phoenix,
            label: "Phoenix (Elixir)",
            description: "Elixir web framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/_build/**",
                "**/deps/**",
                "**/.elixir_ls/**",
                "**/cover/**",
                "**/priv/static/**",
                "**/erl_crash.dump",
            ],
            post_create_cmd: vec!["mix deps.get", "mix ecto.setup"],
            signature: Signature::file_contains("mix.exs", ":phoenix"),
        },
        Preset {
            id: PresetId::Generic,
            label: "Generic",
            description: "Fallback defaults — no specific framework",
            copy_patterns: vec!["**/.env*", ".vscode/**", ".idea/**"],
            copy_ignores: vec![
                "**/node_modules/**",
                "**/dist/**",
                "**/build/**",
                "**/.git/**",
                "**/coverage/**",
                "**/Thumbs.db",
                "**/.DS_Store",
            ],
            post_create_cmd: vec![],
            signature: Signature::never(),
        },
    ]
}

/// Look up a preset by its stable id. `None` when no preset matches.
pub fn find_by_id(id: PresetId) -> Option<Preset> {
    catalog().into_iter().find(|p| p.id == id)
}
