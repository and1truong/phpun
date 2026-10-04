<?php
try { $c = Closure::fromCallable('self::m'); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
