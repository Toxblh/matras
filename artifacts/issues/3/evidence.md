# Issue #3: собственный индикатор присутствия

Issue: https://altlinux.space/toxblh/matras/issues/3 (open)

Индикатор на собственном аватаре теперь использует тот же расчёт размера, что и
аватар в чате: 8 px на аватаре 24 px и 13 px на аватаре 40 px. До исправления
40 px аккаунтный аватар передавал фиксированные 12 px, из-за чего индикаторы
имели разный относительный размер. Красный и зелёный прогоны теста
`account_presence_badges_scale_like_chat_avatar_badges` сохранены в
[test-red.log](test-red.log) и [test-green.log](test-green.log).

Скриншоты детерминированного demo показывают собственный аккаунт и индикатор
во всех обязательных окнах:

- PinePhone 360×720: [state-360x720.png](state-360x720.png)
- Issue-size 720×800: [state-720x800.png](state-720x800.png)
- Desktop 1320×840: [state-1320x840.png](state-1320x840.png)

Все три PNG осмотрены. Серверное состояние присутствия проверяется расчётом
размера, а не скриншотом. Общий suite прошёл: [workspace-test.log](../workspace-test.log).
