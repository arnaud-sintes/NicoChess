plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// One version for everything: the one in Cargo.toml ([workspace.package]). The copy of the project
// the build runs on has it at the root, next to the android folder.
val nicochessVersion: String = rootProject.projectDir.parentFile.resolve("Cargo.toml").readLines()
    .first { it.trim().startsWith("version = \"") }
    .substringAfter('"')
    .substringBefore('"')
val versionNumbers: List<Int> = nicochessVersion.split(".").map { it.toInt() }

android {
    namespace = "com.nicochess.app"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.nicochess.app"
        // Android 7.0: phones and tablets from 2016 on.
        minSdk = 24
        targetSdk = 34
        // 0.2.0 -> 200: grows with the version, as Android requires of an update.
        versionCode = versionNumbers[0] * 10000 + versionNumbers[1] * 100 + versionNumbers[2]
        versionName = nicochessVersion
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions {
        jvmTarget = "17"
    }
}
