# Issue #11: custom status у имени

Issue: https://altlinux.space/toxblh/matras/issues/11 (closed via PR #16)

Custom-status emoji закреплено рядом с именем: имя канала ограничено разумной
шириной, а свободное место остаётся за ним для статуса и trailing-индикаторов.
Так длинное имя не растягивает строку и не отрывает emoji от пользователя.
Тест `channel_name_does_not_expand_status_emoji_away_from_name` подтверждает,
что label больше не запрашивает всю строку; старый `hexpand(true)` воспроизвёл
красный результат. Логи: [test-red.log](test-red.log) и
[test-green.log](test-green.log).

- PinePhone 360×720: [corrected-state-360x720.png](corrected-state-360x720.png)
- Issue-size 720×800: [state-720x800.png](state-720x800.png)
- Desktop 1320×840: [corrected-state-1320x840.png](corrected-state-1320x840.png)

PNG осмотрены; на строке сообщения demo-статус виден вплотную к имени, sidebar
остаётся пригодной для списка длинных названий.
Final combined workspace checks passed: 140 Matras tests, 22 Mattermost API,
36 Mattermost calls, plus doc-tests. See [workspace-test.log](../workspace-test.log).
