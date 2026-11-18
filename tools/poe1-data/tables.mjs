// What export.mjs pulls from the game, shared with the generator.
// Column names: https://github.com/poe-tool-dev/dat-schema

// Languages the poe1 renderer ships (renderer/src/assets/make-index-files.mjs).
export const LANGUAGES = [
  { code: 'en', game: 'English' },
  { code: 'ru', game: 'Russian' },
  { code: 'cmn-Hant', game: 'Traditional Chinese' },
  { code: 'ko', game: 'Korean' }
]

export const TABLES = [
  {
    name: 'Mods',
    columns: ['Id', 'ModTypeKey', 'Domain', 'GenerationType', 'Families', 'SpawnWeight_Values',
      'StatsKey1', 'StatsKey2', 'StatsKey3', 'StatsKey4', 'StatsKey5', 'StatsKey6',
      'Stat1Min', 'Stat1Max', 'Stat2Min', 'Stat2Max', 'Stat3Min', 'Stat3Max',
      'Stat4Min', 'Stat4Max', 'Stat5Min', 'Stat5Max', 'Stat6Min', 'Stat6Max']
  },
  { name: 'Stats', columns: ['Id', 'IsLocal', 'IsWeaponLocal'] },
  { name: 'ModFamily', columns: ['Id'] },
  // UI text the client adds to stat lines (Eldritch "presence" prefixes).
  { name: 'ClientStrings', columns: ['Id', 'Text'] },
  // Base types the renderer's items.ndjson lacks (3.30's Viscera).
  { name: 'BaseItemTypes', columns: ['Id', 'ItemClassesKey', 'Name'] },
  { name: 'ItemClasses', columns: ['Id', 'Name'] }
]
