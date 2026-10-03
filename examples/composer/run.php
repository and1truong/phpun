<?php
require __DIR__ . '/vendor/autoload.php';
echo \Demo\Hello::hi(), "\n";
echo \Legacy_Thing::tag(), "\n";
echo \Mapped_Class::tag(), "\n";
