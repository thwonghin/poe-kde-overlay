// tests/ipc_checks.ts against the poe1 renderer (vendor/awakened-poe-trade).
import type { IpcEvent as Own, HostState } from '../vendor/awakened-poe-trade/ipc/types'
import type { IpcEvent as Other } from '../vendor/exiled-exchange-2/ipc/types'
import type { HostState as OurHostState } from './generated/HostState'
import type * as C from './ipc_checks'

export const unhandled: C.Verdict<C.Unhandled<Own, Other>> = 'ok'
export const unknownIncoming: C.Verdict<C.UnknownIncoming<Own, Other>> = 'ok'
export const badIncoming: C.Verdict<C.BadIncoming<Own>> = 'ok'
export const unknownOutgoing: C.Verdict<C.UnknownOutgoing<Own>> = 'ok'
export const badOutgoing: C.Verdict<C.BadOutgoing<Own>> = 'ok'
// GET /config's body.
export const hostState: HostState = null as unknown as OurHostState
