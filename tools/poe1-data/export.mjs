// Exports the PoE1 game data the generator reads, straight from GGG's patch
// CDN (no game install needed):  node export.mjs <patch>
// Latest patch: https://raw.githubusercontent.com/poe-tool-dev/latest-patch-version/main/latest.txt
// The CDN only keeps each league's last patch and the current one.
// (CLI internals are imported by path: the package only exports its lib.)
// Output: cache/<patch>/{files,tables}/ — what the CLI of pathofexile-dat
// would write, plus every stat description file (listed from the index,
// since their names and extension change between leagues: .txt -> .csd in 3.30).
import * as L from './node_modules/pathofexile-dat/dist/cli/bundle-loaders.js'
import { exportTables } from './node_modules/pathofexile-dat/dist/cli/export-tables.js'
import { readIndexBundle, decompressedBundleSize, decompressSliceInBundle, getDirContent } from 'pathofexile-dat/bundles.js'
import fs from 'node:fs/promises'
import path from 'node:path'
import { TABLES, LANGUAGES } from './tables.mjs'

const patch = process.argv[2]
if (!/^3\.\d+\.\d+/.test(patch ?? '')) {
  console.error('usage: node export.mjs <poe1 patch, e.g. 3.30.0.1.3>')
  process.exit(1)
}
const dir = path.resolve(import.meta.dirname, 'cache', patch)
await fs.mkdir(dir, { recursive: true })

const cdn = await L.CdnBundleLoader.create(path.join(dir, '.cache'), patch)
// The CDN drops some bundles of a superseded patch (3.29.3.6 lost
// statdescriptions_1); the library's loader exits on a 404, so fetch
// through one that throws and report what's missing.
const throwingCdn = {
  async fetchFile (name) {
    const cached = path.join(dir, '.cache', patch, name.replace(/\//g, '@'))
    try { return await fs.readFile(cached) } catch {}
    const res = await fetch(L.toCdnUrl?.(patch, name) ?? `https://patch.poecdn.com/${patch}/Bundles2/${name}`)
    if (!res.ok) throw new Error(`CDN ${res.status}: ${name}`)
    const bin = new Uint8Array(await res.arrayBuffer())
    await fs.writeFile(cached, bin)
    return bin
  }
}
const loader = await L.FileLoader.create(new L.CachingBundleLoader(throwingCdn))

const indexBin = await cdn.fetchFile('_.index.bin')
const index = new Uint8Array(decompressedBundleSize(indexBin))
decompressSliceInBundle(indexBin, 0, index)
const { pathRepsBundle, dirsInfo } = readIndexBundle(index)
const pathReps = new Uint8Array(decompressedBundleSize(pathRepsBundle))
decompressSliceInBundle(pathRepsBundle, 0, pathReps)
// Directory hashes are of the lowercased path.
const descFiles = getDirContent('metadata/statdescriptions', pathReps, dirsInfo).files
  .filter(f => /_descriptions\.(txt|csd)$/.test(f))

const filesDir = path.join(dir, 'files')
await fs.rm(filesDir, { recursive: true, force: true })
await fs.mkdir(filesDir)
const missing = []
for (const file of descFiles) {
  // Stored as UTF-16LE; written out as UTF-8 so the generator reads plain text.
  let utf16
  try { utf16 = await loader.getFileContents(file) } catch (e) { missing.push(`${file} (${e.message})`); continue }
  const text = new TextDecoder('utf-16le').decode(utf16).replace(/^﻿/, '')
  await fs.writeFile(path.join(filesDir, path.basename(file).replace(/\.(txt|csd)$/, '.txt')), text)
}
console.log(`exported ${descFiles.length - missing.length}/${descFiles.length} stat description files`)
if (missing.length) console.warn(`MISSING on CDN:\n  ${missing.join('\n  ')}`)

// One table of one language at a time, so a bundle the CDN dropped (3.29.3.6
// lost Korean ClientStrings) costs only that table. exportTables writes to
// <cwd>/tables regardless of its outDir argument and clears the language's
// folder first, hence the scratch cwd.
const tablesDir = path.join(dir, 'tables')
const scratch = path.join(dir, '.export')
await fs.rm(tablesDir, { recursive: true, force: true })
const missingTables = []
for (const { game } of LANGUAGES) {
  await fs.mkdir(path.join(tablesDir, game), { recursive: true })
  for (const table of TABLES) {
    await fs.rm(scratch, { recursive: true, force: true })
    await fs.mkdir(scratch)
    process.chdir(scratch)
    try {
      await exportTables({ patch, translations: [game], tables: [table] }, path.join(scratch, 'tables'), loader)
      await fs.rename(path.join(scratch, 'tables', game, `${table.name}.json`), path.join(tablesDir, game, `${table.name}.json`))
    } catch (e) {
      missingTables.push(`${game}/${table.name} (${e.message})`)
    }
  }
}
process.chdir(dir)
await fs.rm(scratch, { recursive: true, force: true })
if (missingTables.length) console.warn(`MISSING tables:\n  ${missingTables.join('\n  ')}`)

// The trade site's stat list: which stats it searches, and as what types.
// Live, so only meaningful for the current league's patch.
const res = await fetch('https://www.pathofexile.com/api/trade/data/stats', { headers: { 'User-Agent': 'poe-kde-overlay data generator' } })
if (!res.ok) throw new Error(`trade stats: HTTP ${res.status}`)
await fs.mkdir(path.join(dir, 'trade'), { recursive: true })
await fs.writeFile(path.join(dir, 'trade', 'stats.json'), await res.text())
console.log(`exported to ${dir}`)
