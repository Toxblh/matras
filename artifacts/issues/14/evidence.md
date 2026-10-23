# Issue #14: play-кнопка остаётся на видеопостере

Issue: https://altlinux.space/toxblh/matras/issues/14 (closed via PR #16)

Постер ограничен шириной 420 px, но строка сообщения может быть шире.
Overlay теперь выравнивается по началу poster clamp, поэтому центрированная
play-кнопка остаётся на видео, а не уплывает в пустую область строки. До фикса
GTK allocation-проверка дала центр кнопки x=450 при правой границе poster x=420;
с конфигурацией `Align::Start` тест проходит. См.
[test-red.log](test-red.log) и [test-green.log](test-green.log), а также исходный
геометрический fixture [video-layout.png](video-layout.png).

Для воспроизводимых окон добавлен demo-video с poster frame:

- PinePhone 360×720: [state-360x720.png](state-360x720.png)
- Issue-size 720×1680: [state-720x1680.png](state-720x1680.png)
- Desktop 1320×840: [state-1320x840.png](state-1320x840.png)

На всех размерах play-кнопка видна внутри poster. В issue-size и desktop PNG
видео и строка сообщения полностью видны; на 360 px постер сохраняет
естественную панорамную пропорцию вложенного клипа.
Final combined workspace checks passed: 140 Matras tests, 22 Mattermost API,
36 Mattermost calls, plus doc-tests. See [workspace-test.log](../workspace-test.log).
