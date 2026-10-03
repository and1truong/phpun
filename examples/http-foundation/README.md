# symfony/http-foundation on phpun

An **unmodified** `symfony/http-foundation` install (real `composer
install` vendor tree) bootstrapping and serving a real request flow
under `phpun serve`.

    phpun serve examples/http-foundation/public/index.php \
        --docroot examples/http-foundation/public --port 8080

    curl http://127.0.0.1:8080/                    # <h1>phpun + symfony/http-foundation</h1>
    curl 'http://127.0.0.1:8080/hello?name=hong'   # {"hello":"hong"}
    curl -XPOST -d raw 'http://127.0.0.1:8080/echo?x=1'
    # {"method":"POST","query":{"x":"1"},"body":"raw","ip":"127.0.0.1"}
    curl http://127.0.0.1:8080/nope                # {"error":"not found"} (404)

`Request::createFromGlobals()` reads the request superglobals phpun
populates from the HTTP request (`$_SERVER`, `$_GET`, `$_POST`,
php://input). `Response->send()` emits headers/status through phpun's
`header()` surface.
