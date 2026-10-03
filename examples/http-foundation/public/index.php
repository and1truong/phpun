<?php
// symfony/http-foundation demo — unmodified vendor component on phpun.
require dirname(__DIR__) . '/vendor/autoload.php';

use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;

$request = Request::createFromGlobals();

$response = match (true) {
    $request->getPathInfo() === '/' =>
        new Response('<h1>phpun + symfony/http-foundation</h1>' . "\n"),
    $request->getPathInfo() === '/hello' =>
        new JsonResponse(['hello' => $request->query->get('name', 'world')]),
    $request->getPathInfo() === '/echo' => new JsonResponse([
        'method' => $request->getMethod(),
        'query' => $request->query->all(),
        'body' => $request->getContent(),
        'ip' => $request->getClientIp(),
    ]),
    default => new JsonResponse(['error' => 'not found'], 404),
};

$response->send();
