# Issue #12: расположение панели звонка в мобильной навигации

Issue: https://altlinux.space/toxblh/matras/issues/12 (closed via PR #16)

В свёрнутой навигации панель активного звонка следует за текущей страницей:
она находится у списка, когда открыт список каналов, и у разговора, когда
открыт чат. Desktop оставляет dock в sidebar. Красный тест воспроизводил старое
поведение, привязанное только к collapsed-состоянию; зелёный проверяет, что
dock принадлежит чату лишь при открытом содержимом. Логи:
[test-red.log](test-red.log) и [test-green.log](test-green.log).

- PinePhone 360×720: [corrected-state-360x720.png](corrected-state-360x720.png)
- Issue-size 720×1680: [corrected-state-720x1680.png](corrected-state-720x1680.png)
- Desktop 1320×840: [corrected-state-1320x840.png](corrected-state-1320x840.png)

Скриншоты показывают list/chat/mobile dock placement и desktop sidebar. GTK
поведение проверяется тестом состояния навигации.
Final combined workspace checks passed: 140 Matras tests, 22 Mattermost API,
36 Mattermost calls, plus doc-tests. See [workspace-test.log](../workspace-test.log).
