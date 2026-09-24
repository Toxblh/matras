# Issue #13: состояние кнопки поднятой руки

Issue: https://altlinux.space/toxblh/matras/issues/13 (open)

Кнопка использует символ поднятой руки. Пока рука поднята, кнопка подсвечена и
подсказка предлагает опустить её; после опускания возвращается flat-действие
«Raise your hand». Красный прогон с прежней иконкой сортировки провалил оба
состояния; зелёный подтвердил новое действие. Логи:
[test-red.log](test-red.log) и [test-green.log](test-green.log).

- PinePhone 360×720: [corrected-state-360x720.png](corrected-state-360x720.png)
- Issue-size 720×800: [state-720x800.png](state-720x800.png)
- Desktop 1320×840: [corrected-state-1320x840.png](corrected-state-1320x840.png)

PNG показывают активный звонок и поднятую руку при каждом размере; функциональные
иконка/подсказка и обратное состояние защищены тестами.
Final combined workspace checks passed: 140 Matras tests, 22 Mattermost API,
36 Mattermost calls, plus doc-tests. See [workspace-test.log](../workspace-test.log).
