const { existsSync, readdirSync, readFileSync } = require('node:fs');
const { join, resolve } = require('node:path');

// Run before bundling so unverified native code cannot become a release asset.
try {
  if (process.argv.length !== 3) {
    throw new Error('Usage: node verify-portable-ggml.cjs <target/profile directory>');
  }
  const buildDirectory = resolve(process.argv[2], 'build');
  // Cargo before 1.100: build/whisper-rs-sys-<hash>/out; from 1.100: build/whisper-rs-sys/<hash>/out.
  const subdirs = (dir) => readdirSync(dir, { withFileTypes: true }).filter((e) => e.isDirectory()).map((e) => e.name);
  const outDirs = existsSync(buildDirectory)
    ? subdirs(buildDirectory).flatMap((name) => {
      if (name.startsWith('whisper-rs-sys-')) return [join(buildDirectory, name, 'out')];
      if (name === 'whisper-rs-sys') return subdirs(join(buildDirectory, name)).map((hash) => join(buildDirectory, name, hash, 'out'));
      return [];
    })
    : [];
  const caches = outDirs.map((dir) => join(dir, 'build/CMakeCache.txt')).filter(existsSync);
  if (caches.length === 0) {
    throw new Error(`No Whisper CMake cache found in ${buildDirectory}`);
  }

  for (const cache of caches) {
    const contents = readFileSync(cache, 'utf8');
    console.log(`Checking ${cache}`);
    for (const line of contents.split(/\r?\n/)) {
      if (/^(GGML_(NATIVE|AVX[^:]*|FMA|F16C)|CMAKE_C(?:XX)?_FLAGS[^:]*):/.test(line)) {
        console.log(line);
      }
    }
    for (const flag of ['GGML_NATIVE', 'GGML_AVX512', 'GGML_AVX512_VBMI', 'GGML_AVX512_VNNI', 'GGML_AVX512_BF16']) {
      if (!new RegExp(`^${flag}:BOOL=OFF\\r?$`, 'm').test(contents)) {
        throw new Error(`${flag} must be explicitly OFF in ${cache}`);
      }
    }
  }
  console.log('Verified Windows Whisper CPU portability before bundling.');
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
