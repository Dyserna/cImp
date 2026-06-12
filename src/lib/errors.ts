/// Normalize an unknown thrown value into a displayable message string.
/// Tauri IPC rejects with a plain `string`; other throws may be `Error`
/// instances or arbitrary objects. Strings pass through verbatim; anything
/// else is JSON-stringified so the caller always has something to show.
export function errorMessage(e: unknown): string {
  return typeof e === 'string' ? e : JSON.stringify(e);
}
