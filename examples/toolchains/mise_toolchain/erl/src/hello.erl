-module(hello).
-export([main/1]).

main(_Args) ->
    io:format("Hello from Erlang/OTP ~s~n", [erlang:system_info(otp_release)]).
