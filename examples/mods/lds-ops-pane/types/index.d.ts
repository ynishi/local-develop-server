export type PublicityRow = { platform: string; publicity: string; underlying?: string }

export type Worktree = {
  path: string
  branch: string | null
  head: string | null
  owned: boolean
}

export type JournalHead = { chapter: string; state: string; at: number | null }

export type Snapshot = {
  /** `$.clock.now()` when the snapshot was taken. */
  at: number
  sessionId: string | null
  root: string | null
  level: 'public' | 'private' | 'unknown'
  publicity: PublicityRow[]
  worktrees: Worktree[]
  journal: JournalHead | null
  /** One line per source that could not be read ("publicity: lds not connected"). */
  errors: string[]
}

declare module 'claude-code' {
  interface PluginState {
    'lds-ops-pane': { snapshot: Snapshot | null; isBandHidden: boolean }
  }
}
