/* Точка входа для GTK Android runtime.
 *
 * gdkandroidruntime.c делает g_module_open на lib<таргет>.so и ищет
 * g_module_symbol(app, "main", ...). Символ должен называться ровно "main",
 * лежать в .dynsym и НЕ возвращаться: возврат отсюда убивает процесс.
 *
 * Rust-сторона (crates/matras/src/lib.rs):
 *     #[unsafe(no_mangle)]
 *     pub extern "C" fn matras_main(argc: c_int, argv: *mut *mut c_char,
 *                                   envp: *mut *mut c_char) -> c_int
 *
 * Нужен только для варианта со staticlib. Если поедет cdylib напрямую
 * (meson 1.11, pkg.executable), символ main экспортируется из Rust и шим не нужен.
 */
extern int matras_main (int argc, char **argv, char **envp);

int
main (int argc, char **argv, char **envp)
{
  return matras_main (argc, argv, envp);
}
