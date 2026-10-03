<?php
require __DIR__ . '/vendor/autoload.php';

use Symfony\Component\Console\Application;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Input\InputArgument;
use Symfony\Component\Console\Input\InputInterface;
use Symfony\Component\Console\Input\InputOption;
use Symfony\Component\Console\Output\OutputInterface;
use Symfony\Component\Console\Style\SymfonyStyle;

class GreetCommand extends Command {
    protected function configure(): void {
        $this->setName('app:greet')
            ->setDescription('Greet a user')
            ->addArgument('name', InputArgument::REQUIRED, 'Who to greet')
            ->addOption('yell', 'y', InputOption::VALUE_NONE, 'Uppercase the greeting')
            ->addOption('iterations', 'i', InputOption::VALUE_REQUIRED, 'Repeat count', '1');
    }

    protected function execute(InputInterface $input, OutputInterface $output): int {
        $io = new SymfonyStyle($input, $output);
        $name = $input->getArgument('name');
        $io->title('phpun + symfony/console');
        for ($i = 0; $i < (int) $input->getOption('iterations'); $i++) {
            $greeting = "Hello, {$name}!";
            if ($input->getOption('yell')) {
                $greeting = strtoupper($greeting);
            }
            $io->writeln($greeting);
        }
        $io->success('Command finished');
        return Command::SUCCESS;
    }
}

$app = new Application('phpun-console', '1.0.0');
$app->add(new GreetCommand());
exit($app->run(new \Symfony\Component\Console\Input\ArgvInput()));
