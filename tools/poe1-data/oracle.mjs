// Builds the comparison oracle: the game data upstream generated its
// committed stats from, as close as the CDN allows.
//
//   node oracle.mjs <old patch> <current patch> <repoe-fork commit>
//
// <old patch> is the previous league's last patch (the CDN keeps it, but
// drops some of its bundles, e.g. 3.29.3.6 lost a third of the stat
// description files). Files it lacks come from RePoE's export at the patch
// upstream used (repoe-fork/repoe-fork.github.io commit "Export <patch>",
// stat translations converted back to the game's text), and only then from
// <current patch>. Both patches must be exported first (export.mjs).
// Output: cache/oracle/{files,tables} for `poe1-data compare cache/oracle`.
import fs from 'node:fs/promises'
import path from 'node:path'

const [oldPatch, curPatch, repoe] = process.argv.slice(2)
if (!repoe) {
  console.error('usage: node oracle.mjs <old patch> <current patch> <repoe-fork commit>')
  process.exit(1)
}
const cache = path.join(import.meta.dirname, 'cache')
const out = path.join(cache, 'oracle')
await fs.rm(out, { recursive: true, force: true })
await fs.mkdir(path.join(out, 'files'), { recursive: true })
await fs.symlink(path.join('..', oldPatch, 'tables'), path.join(out, 'tables'))

// RePoE names a file after the game's, minus the suffix; map is "areas".
const repoeName = file => {
  const stem = file.replace(/_stat_descriptions\.txt$/, '')
  return stem === 'map' ? 'areas' : stem
}

// RePoE's stat translation entry -> a game description block.
function block (entry) {
  const lines = ['description', `\t${entry.ids.length} ${entry.ids.join(' ')}`, `\t${entry.English.length}`]
  for (const v of entry.English) {
    const conds = v.condition.map(c => {
      if (c.negated) return `!${c.min}`
      if (c.min == null && c.max == null) return '#'
      if (c.min === c.max) return `${c.min}`
      return `${c.min ?? '#'}|${c.max ?? '#'}`
    })
    const text = v.string
      .replace(/\{(\d+)\}/g, (s, i) => v.format[i] === '+#' ? `{${i}:+d}` : s)
      .replace(/\n/g, '\\n')
    const flags = v.index_handlers.flatMap((hs, i) => hs.map(h => `${h} ${i + 1}`))
    lines.push(`\t\t${conds.join(' ')} "${text}"${flags.length ? ' ' + flags.join(' ') : ''}`)
  }
  return lines.join('\n')
}

const have = new Set(await fs.readdir(path.join(cache, oldPatch, 'files')))
const from = { [oldPatch]: 0, repoe: 0, [curPatch]: 0 }
for (const file of await fs.readdir(path.join(cache, curPatch, 'files'))) {
  const dest = path.join(out, 'files', file)
  if (have.has(file)) {
    await fs.copyFile(path.join(cache, oldPatch, 'files', file), dest)
    from[oldPatch]++
    continue
  }
  const res = await fetch(`https://raw.githubusercontent.com/repoe-fork/repoe-fork.github.io/${repoe}/data/stat_translations/${repoeName(file)}.json`)
  const current = await fs.readFile(path.join(cache, curPatch, 'files', file), 'utf8')
  if (!res.ok) {
    await fs.writeFile(dest, current)
    from[curPatch]++
    console.warn(`  ${file}: not in RePoE, using ${curPatch}`)
    continue
  }
  // Keep the file's include, which RePoE's export drops.
  const include = /^include .*$/m.exec(current)?.[0]
  const blocks = (await res.json()).map(block)
  await fs.writeFile(dest, [include, ...blocks].filter(Boolean).join('\n') + '\n')
  from.repoe++
}
console.log('stat description files from:', from)
