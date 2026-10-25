-module(shapes).
-export([area/1]).
-record(point, {x = 0, y = 0}).
area({square, S}) -> S * S;
area({circle, R}) -> math:pi() * R * R.
