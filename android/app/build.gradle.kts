plugins {
    id("com.android.application")
}

android {
    namespace = "io.github.acus9345.singboxwatch"
    compileSdk = 35

    defaultConfig {
        applicationId = "io.github.acus9345.singboxwatch"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "1.0.0"
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"))
            // sideloaded app: sign the release with the debug key so the
            // .apk installs as-is (no keystore to manage)
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}
