<?php
require __DIR__ . '/vendor/autoload.php';

use Doctrine\Inflector\InflectorFactory;

$inflector = InflectorFactory::create()->build();

$words = ['child', 'octopus', 'equipment', 'information', 'UserProfile'];
foreach ($words as $w) {
    printf("%-14s pluralize=%-16s tableize=%-16s\n", $w, $inflector->pluralize($w), $inflector->tableize($w));
}
foreach (['children', 'octopi', 'user_profiles'] as $w) {
    printf("%-14s singularize=%-16s classify=%-16s\n", $w, $inflector->singularize($w), $inflector->classify($w));
}
printf("camelize:   %s\n", $inflector->camelize('user_profile_name'));
printf("capitalize: %s\n", $inflector->capitalize('schiß straight'));
printf("urlize:     %s\n", $inflector->urlize('Foo Bar: Just use'));
printf("seemsUtf8:  %s/%s\n", $inflector->seemsUtf8('héllo') ? 'T' : 'F', $inflector->seemsUtf8("abc\x80") ? 'T' : 'F');
echo $inflector->unaccent('ài chỉ là'), "\n";
echo $inflector->unaccent('schiß'), "\n";
