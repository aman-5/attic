<?php
namespace App\Services;

use Vendor\Package\BaseWorker;
use Vendor\Package\{Formatter, LoggerTrait as Logger};

require_once "../bootstrap.php";
include "helpers.php";

trait Decorates {
    public function decorate(string $name): string {
        return helper($name);
    }
}

interface Runnable {
    public function run(): void;
}

enum Status: string {
    case Ready = "ready";
}

function helper(string $name): string {
    return strtoupper($name);
}

class Worker extends BaseWorker implements Runnable {
    use Decorates;

    public function run(): void {
        helper("job");
        $this->decorate("job");
        Logger::boot();
    }
}
