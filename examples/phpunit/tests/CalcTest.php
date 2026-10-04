<?php
declare(strict_types=1);
use PHPUnit\Framework\TestCase;

final class CalcTest extends TestCase
{
    public function testAddition(): void
    {
        $this->assertSame(3, 1 + 2);
    }

    public function testTruth(): void
    {
        $this->assertTrue(true);
        $this->assertFalse(false);
        $this->assertEquals('a' . 'b', 'ab');
    }

    public function testArray(): void
    {
        $this->assertEquals([1, 2, 3], [1, 2, 3]);
        $this->assertCount(2, ['a', 'b']);
    }
}
