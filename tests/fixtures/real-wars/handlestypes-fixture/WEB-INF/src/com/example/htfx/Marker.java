package com.example.htfx;

/**
 * Marker interface used as the {@code @HandlesTypes} target by
 * {@link HandlesTypesSci}.
 *
 * <p>Two implementations, {@link AlphaImpl} and {@link BetaImpl}, live
 * in the same package — both must show up in the {@code Set<Class<?>>}
 * the container passes into {@code onStartup}.
 */
public interface Marker {
}
