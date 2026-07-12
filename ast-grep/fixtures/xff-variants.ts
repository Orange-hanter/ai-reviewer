/** Eval fixture: все варианты сырого X-Forwarded-For, которые ловит raw-x-forwarded-for. */
import type { Request } from 'express';

export function probeXff(req: Request) {
  return [
    req.header('x-forwarded-for'),
    req.header("x-forwarded-for"),
    req.header('X-Forwarded-For'),
    req.header("X-Forwarded-For"),
    req.get('x-forwarded-for'),
    req.get("x-forwarded-for"),
    req.get('X-Forwarded-For'),
    req.get("X-Forwarded-For"),
    req.headers['x-forwarded-for'],
    req.headers["x-forwarded-for"],
    req.headers['X-Forwarded-For'],
    req.headers["X-Forwarded-For"],
  ];
}
