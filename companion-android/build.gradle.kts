// The app itself has no network code; these repositories are used at build
// time only (SPEC.md 1.3 forbids anything the *product* fetches at runtime).
plugins {
    id("com.android.application") version "8.13.0" apply false
    id("org.jetbrains.kotlin.android") version "2.2.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.20" apply false
}
